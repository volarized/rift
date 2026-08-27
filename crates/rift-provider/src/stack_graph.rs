//! Stack Graph name binding from syntax-derived facts.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, ContributionReference, DeclarationBinding,
    PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId, ReferenceRole,
    SemanticReference, SourceApplicability, SourceKind, SourceLocation, SourceRevision,
    SourceUnitId, TreeRevision,
};
use stack_graphs::NoCancellation;
use stack_graphs::arena::Handle;
use stack_graphs::graph::{Node, StackGraph};
use stack_graphs::partial::PartialPaths;
use stack_graphs::stitching::{ForwardPartialPathStitcher, GraphEdgeCandidates, StitcherConfig};

use crate::{
    AdapterPublication, ProviderInputMode, ProviderPublication, PublicationCoverage,
    PublicationLimits,
};

/// Stack Graph conversion failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackGraphViolation {
    /// Input carries no scope, definition, or reference facts.
    EmptyInput,
    /// Input repeats one provider-local symbol.
    DuplicateSymbol,
    /// Definition, reference, or nested scope names an absent scope.
    MissingScope,
    /// Scope parent links contain a cycle.
    ScopeCycle,
    /// Stack Graph node construction failed.
    InvalidGraph,
    /// Contribution validation failed.
    InvalidContribution,
    /// Provider publication validation failed.
    InvalidPublication,
    /// Source-unit coverage validation failed.
    InvalidCoverage,
}

/// Error returned by Stack Graph conversion.
#[derive(Debug)]
pub struct StackGraphAdapterError {
    violation: StackGraphViolation,
    detail: String,
}

impl StackGraphAdapterError {
    /// Returns stable violation.
    #[must_use]
    pub const fn violation(&self) -> StackGraphViolation {
        self.violation
    }

    /// Returns failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for StackGraphAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Stack Graph adapter rejected {:?}: {}",
            self.violation, self.detail
        )
    }
}

impl Error for StackGraphAdapterError {}

/// One syntax-derived scope.
#[derive(Debug, Clone)]
pub struct StackGraphScope {
    symbol: ProviderSymbolId,
    facts: PortableSymbolFacts,
    source: DeclarationBinding,
    parent: Option<ProviderSymbolId>,
}

impl StackGraphScope {
    /// Creates one scope fact.
    #[must_use]
    pub const fn new(
        symbol: ProviderSymbolId,
        facts: PortableSymbolFacts,
        source: DeclarationBinding,
        parent: Option<ProviderSymbolId>,
    ) -> Self {
        Self {
            symbol,
            facts,
            source,
            parent,
        }
    }

    /// Returns provider-local symbol identity.
    #[must_use]
    pub const fn symbol(&self) -> &ProviderSymbolId {
        &self.symbol
    }

    /// Returns portable symbol facts.
    #[must_use]
    pub const fn facts(&self) -> &PortableSymbolFacts {
        &self.facts
    }

    /// Returns exact source binding.
    #[must_use]
    pub const fn source(&self) -> &DeclarationBinding {
        &self.source
    }

    /// Returns containing scope.
    #[must_use]
    pub const fn parent(&self) -> Option<&ProviderSymbolId> {
        self.parent.as_ref()
    }
}

/// One syntax-derived definition.
#[derive(Debug, Clone)]
pub struct StackGraphDefinition {
    symbol: ProviderSymbolId,
    facts: PortableSymbolFacts,
    source: DeclarationBinding,
    scope: ProviderSymbolId,
}

impl StackGraphDefinition {
    /// Creates one definition fact.
    #[must_use]
    pub const fn new(
        symbol: ProviderSymbolId,
        facts: PortableSymbolFacts,
        source: DeclarationBinding,
        scope: ProviderSymbolId,
    ) -> Self {
        Self {
            symbol,
            facts,
            source,
            scope,
        }
    }

    /// Returns provider-local symbol identity.
    #[must_use]
    pub const fn symbol(&self) -> &ProviderSymbolId {
        &self.symbol
    }

    /// Returns portable symbol facts.
    #[must_use]
    pub const fn facts(&self) -> &PortableSymbolFacts {
        &self.facts
    }

    /// Returns exact source binding.
    #[must_use]
    pub const fn source(&self) -> &DeclarationBinding {
        &self.source
    }

    /// Returns containing scope.
    #[must_use]
    pub const fn scope(&self) -> &ProviderSymbolId {
        &self.scope
    }
}

/// One syntax-derived reference.
#[derive(Debug, Clone)]
pub struct StackGraphReference {
    symbol: ProviderSymbolId,
    facts: PortableSymbolFacts,
    source: DeclarationBinding,
    scope: ProviderSymbolId,
    role: ReferenceRole,
}

impl StackGraphReference {
    /// Creates one reference fact.
    #[must_use]
    pub const fn new(
        symbol: ProviderSymbolId,
        facts: PortableSymbolFacts,
        source: DeclarationBinding,
        scope: ProviderSymbolId,
        role: ReferenceRole,
    ) -> Self {
        Self {
            symbol,
            facts,
            source,
            scope,
            role,
        }
    }

    /// Returns provider-local symbol identity.
    #[must_use]
    pub const fn symbol(&self) -> &ProviderSymbolId {
        &self.symbol
    }

    /// Returns portable symbol facts.
    #[must_use]
    pub const fn facts(&self) -> &PortableSymbolFacts {
        &self.facts
    }

    /// Returns exact source binding.
    #[must_use]
    pub const fn source(&self) -> &DeclarationBinding {
        &self.source
    }

    /// Returns containing scope.
    #[must_use]
    pub const fn scope(&self) -> &ProviderSymbolId {
        &self.scope
    }

    /// Returns portable reference role.
    #[must_use]
    pub const fn role(&self) -> ReferenceRole {
        self.role
    }
}

/// Syntax-derived inputs for one Stack Graph publication.
#[derive(Debug, Clone)]
pub struct StackGraphInput {
    scopes: Vec<StackGraphScope>,
    definitions: Vec<StackGraphDefinition>,
    references: Vec<StackGraphReference>,
}

impl StackGraphInput {
    /// Creates one input set.
    #[must_use]
    pub const fn new(
        scopes: Vec<StackGraphScope>,
        definitions: Vec<StackGraphDefinition>,
        references: Vec<StackGraphReference>,
    ) -> Self {
        Self {
            scopes,
            definitions,
            references,
        }
    }

    /// Returns scope facts.
    #[must_use]
    pub fn scopes(&self) -> &[StackGraphScope] {
        &self.scopes
    }

    /// Returns definition facts.
    #[must_use]
    pub fn definitions(&self) -> &[StackGraphDefinition] {
        &self.definitions
    }

    /// Returns reference facts.
    #[must_use]
    pub fn references(&self) -> &[StackGraphReference] {
        &self.references
    }
}

/// Converts declarative syntax facts into name-binding Contributions.
#[derive(Debug, Clone)]
pub struct StackGraphAdapter {
    provider: ProviderId,
    publication_revision: ProviderRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    limits: PublicationLimits,
}

impl StackGraphAdapter {
    /// Creates one fact adapter.
    #[must_use]
    pub const fn new(
        provider: ProviderId,
        publication_revision: ProviderRevision,
        source_revision: SourceRevision,
        tree_revision: TreeRevision,
        limits: PublicationLimits,
    ) -> Self {
        Self {
            provider,
            publication_revision,
            source_revision,
            tree_revision,
            limits,
        }
    }

    /// Converts syntax-derived scopes, definitions, and references.
    ///
    /// # Errors
    ///
    /// Returns `StackGraphAdapterError` when scope links, graph construction,
    /// Contributions, publication, or coverage are invalid.
    pub fn convert(
        &self,
        input: &StackGraphInput,
    ) -> Result<AdapterPublication, StackGraphAdapterError> {
        Self::validate_input(input, self.limits.contributions_per_provider_max())?;
        let resolved = self.resolve(input)?;
        let contributions = self.build_contributions(input, &resolved)?;
        let publication = ProviderPublication::new(
            self.provider.clone(),
            self.publication_revision,
            contributions,
            self.limits,
        )
        .map_err(|error| {
            stack_graph_error(StackGraphViolation::InvalidPublication, error.to_string())
        })?;
        let mut units = source_units(input);
        units.sort();
        units.dedup();
        AdapterPublication::new(
            ProviderInputMode::Fact,
            PublicationCoverage::SourceUnits(units),
            publication,
        )
        .map_err(|error| stack_graph_error(StackGraphViolation::InvalidCoverage, error.to_string()))
    }

    fn validate_input(
        input: &StackGraphInput,
        facts_max: usize,
    ) -> Result<(), StackGraphAdapterError> {
        if input.scopes.is_empty() && input.definitions.is_empty() && input.references.is_empty() {
            return Err(stack_graph_error(StackGraphViolation::EmptyInput, "facts"));
        }
        let facts = input
            .scopes
            .len()
            .checked_add(input.definitions.len())
            .and_then(|count| count.checked_add(input.references.len()))
            .ok_or_else(|| stack_graph_error(StackGraphViolation::InvalidPublication, "facts"))?;
        if facts > facts_max {
            return Err(stack_graph_error(
                StackGraphViolation::InvalidPublication,
                "facts",
            ));
        }
        let mut symbols = BTreeSet::new();
        for symbol in input
            .scopes
            .iter()
            .map(|fact| &fact.symbol)
            .chain(input.definitions.iter().map(|fact| &fact.symbol))
            .chain(input.references.iter().map(|fact| &fact.symbol))
        {
            if !symbols.insert(symbol) {
                return Err(stack_graph_error(
                    StackGraphViolation::DuplicateSymbol,
                    symbol.to_string(),
                ));
            }
        }

        let scopes = input
            .scopes
            .iter()
            .map(|scope| (&scope.symbol, scope))
            .collect::<BTreeMap<_, _>>();
        for scope in &input.scopes {
            if let Some(parent) = &scope.parent {
                require_scope(&scopes, parent)?;
            }
            let mut visited = BTreeSet::new();
            let mut current = Some(&scope.symbol);
            while let Some(symbol) = current {
                if !visited.insert(symbol) {
                    return Err(stack_graph_error(
                        StackGraphViolation::ScopeCycle,
                        scope.symbol.to_string(),
                    ));
                }
                current = scopes.get(symbol).and_then(|value| value.parent.as_ref());
            }
        }
        for definition in &input.definitions {
            require_scope(&scopes, &definition.scope)?;
        }
        for reference in &input.references {
            require_scope(&scopes, &reference.scope)?;
        }
        Ok(())
    }

    fn resolve(
        &self,
        input: &StackGraphInput,
    ) -> Result<BTreeMap<ProviderSymbolId, Vec<ContributionReference>>, StackGraphAdapterError>
    {
        let mut graph = StackGraph::new();
        let mut scope_nodes = BTreeMap::new();
        for scope in &input.scopes {
            let file = graph.get_or_create_file(&scope.source.unit().to_string());
            let node_id = graph.new_node_id(file);
            let node = graph.add_scope_node(node_id, false).ok_or_else(|| {
                stack_graph_error(StackGraphViolation::InvalidGraph, scope.symbol.to_string())
            })?;
            scope_nodes.insert(scope.symbol.clone(), node);
        }
        for scope in &input.scopes {
            let source = scope_nodes[&scope.symbol];
            let sink = scope
                .parent
                .as_ref()
                .map_or(StackGraph::root_node(), |parent| scope_nodes[parent]);
            graph.add_edge(source, sink, 0);
        }

        let mut definition_nodes = BTreeMap::<Handle<Node>, ContributionReference>::new();
        for definition in &input.definitions {
            let file = graph.get_or_create_file(&definition.source.unit().to_string());
            let stack_symbol = graph.add_symbol(definition.facts.name());
            let node_id = graph.new_node_id(file);
            let node = graph
                .add_pop_symbol_node(node_id, stack_symbol, true)
                .ok_or_else(|| {
                    stack_graph_error(
                        StackGraphViolation::InvalidGraph,
                        definition.symbol.to_string(),
                    )
                })?;
            graph.add_edge(scope_nodes[&definition.scope], node, 0);
            graph.add_edge(StackGraph::root_node(), node, 0);
            definition_nodes.insert(
                node,
                ContributionReference::new(self.provider.clone(), definition.symbol.clone()),
            );
        }

        let mut reference_nodes = BTreeMap::<Handle<Node>, ProviderSymbolId>::new();
        for reference in &input.references {
            let file = graph.get_or_create_file(&reference.source.unit().to_string());
            let stack_symbol = graph.add_symbol(reference.facts.name());
            let node_id = graph.new_node_id(file);
            let node = graph
                .add_push_symbol_node(node_id, stack_symbol, true)
                .ok_or_else(|| {
                    stack_graph_error(
                        StackGraphViolation::InvalidGraph,
                        reference.symbol.to_string(),
                    )
                })?;
            graph.add_edge(node, scope_nodes[&reference.scope], 0);
            reference_nodes.insert(node, reference.symbol.clone());
        }

        let starting_nodes = reference_nodes.keys().copied().collect::<Vec<_>>();
        let mut resolved = reference_nodes
            .values()
            .cloned()
            .map(|symbol| (symbol, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        let mut partials = PartialPaths::new();
        ForwardPartialPathStitcher::find_all_complete_partial_paths(
            &mut GraphEdgeCandidates::new(&graph, &mut partials, None),
            starting_nodes,
            StitcherConfig::default(),
            &NoCancellation,
            |_, _, path| {
                let Some(reference) = reference_nodes.get(&path.start_node) else {
                    return;
                };
                let Some(definition) = definition_nodes.get(&path.end_node) else {
                    return;
                };
                if let Some(targets) = resolved.get_mut(reference) {
                    targets.insert(definition.clone());
                }
            },
        )
        .map_err(|error| stack_graph_error(StackGraphViolation::InvalidGraph, error.to_string()))?;
        Ok(resolved
            .into_iter()
            .map(|(symbol, targets)| (symbol, targets.into_iter().collect()))
            .collect())
    }

    fn build_contributions(
        &self,
        input: &StackGraphInput,
        resolved: &BTreeMap<ProviderSymbolId, Vec<ContributionReference>>,
    ) -> Result<Vec<Contribution>, StackGraphAdapterError> {
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .map_err(|error| {
            stack_graph_error(StackGraphViolation::InvalidContribution, error.to_string())
        })?;
        let applicability = SourceApplicability::Exact {
            source_revision: self.source_revision,
            tree_revision: self.tree_revision,
        };
        let mut contributions = Vec::with_capacity(
            input.scopes.len() + input.definitions.len() + input.references.len(),
        );

        for scope in &input.scopes {
            let facts = scope.parent.as_ref().map_or_else(
                || scope.facts.clone(),
                |parent| {
                    scope.facts.clone().container(ContributionReference::new(
                        self.provider.clone(),
                        parent.clone(),
                    ))
                },
            );
            contributions.push(self.build_contribution(
                scope.symbol.clone(),
                facts,
                scope.source.clone(),
                applicability,
                origin.clone(),
                Vec::new(),
            )?);
        }
        for definition in &input.definitions {
            contributions.push(
                self.build_contribution(
                    definition.symbol.clone(),
                    definition
                        .facts
                        .clone()
                        .container(ContributionReference::new(
                            self.provider.clone(),
                            definition.scope.clone(),
                        )),
                    definition.source.clone(),
                    applicability,
                    origin.clone(),
                    Vec::new(),
                )?,
            );
        }
        for reference in &input.references {
            let targets = resolved.get(&reference.symbol).ok_or_else(|| {
                stack_graph_error(
                    StackGraphViolation::InvalidGraph,
                    reference.symbol.to_string(),
                )
            })?;
            let references = if targets.is_empty() {
                Vec::new()
            } else {
                vec![
                    SemanticReference::new(
                        reference.source.clone(),
                        reference.role,
                        targets.clone(),
                    )
                    .map_err(|error| {
                        stack_graph_error(
                            StackGraphViolation::InvalidContribution,
                            error.to_string(),
                        )
                    })?,
                ]
            };
            contributions.push(
                self.build_contribution(
                    reference.symbol.clone(),
                    reference
                        .facts
                        .clone()
                        .container(ContributionReference::new(
                            self.provider.clone(),
                            reference.scope.clone(),
                        )),
                    reference.source.clone(),
                    applicability,
                    origin.clone(),
                    references,
                )?,
            );
        }
        Ok(contributions)
    }

    fn build_contribution(
        &self,
        symbol: ProviderSymbolId,
        facts: PortableSymbolFacts,
        source: DeclarationBinding,
        applicability: SourceApplicability,
        origin: ContributionOrigin,
        references: Vec<SemanticReference>,
    ) -> Result<Contribution, StackGraphAdapterError> {
        Contribution::builder(
            ContributionKey::new(self.provider.clone(), self.publication_revision, symbol),
            applicability,
            facts,
            origin,
        )
        .source(source)
        .references(references)
        .build()
        .map_err(|error| {
            stack_graph_error(StackGraphViolation::InvalidContribution, error.to_string())
        })
    }
}

fn require_scope(
    scopes: &BTreeMap<&ProviderSymbolId, &StackGraphScope>,
    symbol: &ProviderSymbolId,
) -> Result<(), StackGraphAdapterError> {
    if scopes.contains_key(symbol) {
        Ok(())
    } else {
        Err(stack_graph_error(
            StackGraphViolation::MissingScope,
            symbol.to_string(),
        ))
    }
}

fn source_units(input: &StackGraphInput) -> Vec<SourceUnitId> {
    input
        .scopes
        .iter()
        .map(|fact| fact.source.unit().clone())
        .chain(
            input
                .definitions
                .iter()
                .map(|fact| fact.source.unit().clone()),
        )
        .chain(
            input
                .references
                .iter()
                .map(|fact| fact.source.unit().clone()),
        )
        .collect()
}

fn stack_graph_error(
    violation: StackGraphViolation,
    detail: impl Into<String>,
) -> StackGraphAdapterError {
    StackGraphAdapterError {
        violation,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use rift_core::{
        DeclarationBinding, ExactKind, Language, PortableSymbolFacts, ProviderId, ProviderRevision,
        ProviderSymbolId, ReferenceRole, SourceRange, SourceRevision, SourceUnitId, SymbolFacet,
        TreeRevision,
    };

    use super::{
        StackGraphAdapter, StackGraphDefinition, StackGraphInput, StackGraphReference,
        StackGraphScope, StackGraphViolation,
    };
    use crate::{ProviderInputMode, PublicationLimits};

    fn symbol(value: &str) -> ProviderSymbolId {
        ProviderSymbolId::new(value).expect("provider symbol")
    }

    fn binding(path: &str, start: u64) -> DeclarationBinding {
        let unit = format!("rift://source/project/{path}");
        DeclarationBinding::new(
            SourceUnitId::parse(&unit).expect("source unit"),
            SourceRange::new(start, start + 4).expect("range"),
            None,
        )
    }

    fn facts(name: &str, kind: &str, facets: Vec<SymbolFacet>) -> PortableSymbolFacts {
        PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            name,
            format!("crate::{name}"),
            ExactKind(kind.to_owned()),
        )
        .facets(facets)
    }

    fn scope(id: &str, path: &str) -> StackGraphScope {
        StackGraphScope::new(
            symbol(id),
            facts(id, "stack_graph.scope", vec![SymbolFacet::Namespace]),
            binding(path, 0),
            None,
        )
    }

    fn definition(id: &str, name: &str, path: &str) -> StackGraphDefinition {
        StackGraphDefinition::new(
            symbol(id),
            facts(
                name,
                "stack_graph.definition",
                vec![SymbolFacet::Callable, SymbolFacet::Value],
            ),
            binding(path, 8),
            symbol("workspace"),
        )
    }

    fn definition_in(id: &str, name: &str, path: &str, scope: &str) -> StackGraphDefinition {
        StackGraphDefinition::new(
            symbol(id),
            facts(
                name,
                "stack_graph.definition",
                vec![SymbolFacet::Callable, SymbolFacet::Value],
            ),
            binding(path, 8),
            symbol(scope),
        )
    }

    fn reference(id: &str, name: &str, path: &str) -> StackGraphReference {
        StackGraphReference::new(
            symbol(id),
            facts(name, "stack_graph.reference", vec![SymbolFacet::Value]),
            binding(path, 16),
            symbol("workspace"),
            ReferenceRole::Call,
        )
    }

    fn reference_in(id: &str, name: &str, path: &str, scope: &str) -> StackGraphReference {
        StackGraphReference::new(
            symbol(id),
            facts(name, "stack_graph.reference", vec![SymbolFacet::Value]),
            binding(path, 16),
            symbol(scope),
            ReferenceRole::Call,
        )
    }

    fn adapter() -> StackGraphAdapter {
        StackGraphAdapter::new(
            ProviderId::new("stack_graph").expect("provider"),
            ProviderRevision::new(1).expect("publication"),
            SourceRevision::new(2).expect("source"),
            TreeRevision::new(3).expect("tree"),
            PublicationLimits::default(),
        )
    }

    #[test]
    fn cross_file_reference_resolves_without_language_engine() {
        let input = StackGraphInput::new(
            vec![scope("lib", "src/lib.rs"), scope("main", "src/main.rs")],
            vec![definition_in("def.run", "run", "src/lib.rs", "lib")],
            vec![reference_in("ref.run", "run", "src/main.rs", "main")],
        );
        let output = adapter().convert(&input).expect("Stack Graph publication");

        assert_eq!(output.mode(), ProviderInputMode::Fact);
        let publication = output.publication();
        assert_eq!(publication.contributions().len(), 4);
        let reference = publication
            .contributions()
            .iter()
            .find(|contribution| contribution.key().reference().symbol() == &symbol("ref.run"))
            .expect("reference Contribution");
        assert_eq!(reference.references().len(), 1);
        assert_eq!(
            reference.references()[0].targets()[0].symbol(),
            &symbol("def.run")
        );
    }

    #[test]
    fn ambiguous_paths_retain_every_definition() {
        let input = StackGraphInput::new(
            vec![
                scope("one", "src/one.rs"),
                scope("two", "src/two.rs"),
                scope("main", "src/main.rs"),
            ],
            vec![
                definition_in("def.one", "run", "src/one.rs", "one"),
                definition_in("def.two", "run", "src/two.rs", "two"),
            ],
            vec![reference_in("ref.run", "run", "src/main.rs", "main")],
        );
        let output = adapter().convert(&input).expect("Stack Graph publication");
        let reference = output
            .publication()
            .contributions()
            .iter()
            .find(|contribution| contribution.key().reference().symbol() == &symbol("ref.run"))
            .expect("reference Contribution");
        let targets = reference.references()[0]
            .targets()
            .iter()
            .map(|target| target.symbol().clone())
            .collect::<Vec<_>>();
        assert_eq!(targets, vec![symbol("def.one"), symbol("def.two")]);
    }

    #[test]
    fn unresolved_reference_remains_a_contribution() {
        let input = StackGraphInput::new(
            vec![scope("workspace", "src/lib.rs")],
            Vec::new(),
            vec![reference("ref.missing", "missing", "src/main.rs")],
        );
        let output = adapter().convert(&input).expect("Stack Graph publication");
        let reference = &output.publication().contributions()[1];
        assert_eq!(reference.key().reference().symbol(), &symbol("ref.missing"));
        assert!(reference.references().is_empty());
    }

    #[test]
    fn invalid_scope_graph_is_refused() {
        let missing = StackGraphInput::new(
            vec![scope("workspace", "src/lib.rs")],
            vec![StackGraphDefinition::new(
                symbol("def.run"),
                facts("run", "stack_graph.definition", vec![SymbolFacet::Value]),
                binding("src/lib.rs", 8),
                symbol("missing"),
            )],
            Vec::new(),
        );
        assert_eq!(
            adapter()
                .convert(&missing)
                .expect_err("missing scope")
                .violation(),
            StackGraphViolation::MissingScope
        );

        let cycle = StackGraphInput::new(
            vec![
                StackGraphScope::new(
                    symbol("one"),
                    facts("one", "stack_graph.scope", vec![SymbolFacet::Namespace]),
                    binding("src/one.rs", 0),
                    Some(symbol("two")),
                ),
                StackGraphScope::new(
                    symbol("two"),
                    facts("two", "stack_graph.scope", vec![SymbolFacet::Namespace]),
                    binding("src/two.rs", 0),
                    Some(symbol("one")),
                ),
            ],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            adapter()
                .convert(&cycle)
                .expect_err("scope cycle")
                .violation(),
            StackGraphViolation::ScopeCycle
        );
    }

    #[test]
    fn empty_and_duplicate_inputs_are_refused() {
        let empty = StackGraphInput::new(Vec::new(), Vec::new(), Vec::new());
        assert_eq!(
            adapter()
                .convert(&empty)
                .expect_err("empty input")
                .violation(),
            StackGraphViolation::EmptyInput
        );

        let duplicate = StackGraphInput::new(
            vec![scope("workspace", "src/lib.rs")],
            vec![definition("workspace", "run", "src/lib.rs")],
            Vec::new(),
        );
        assert_eq!(
            adapter()
                .convert(&duplicate)
                .expect_err("duplicate symbol")
                .violation(),
            StackGraphViolation::DuplicateSymbol
        );
    }

    #[test]
    fn inputs_expose_syntax_facts_and_nested_scope() {
        let workspace = scope("workspace", "src/lib.rs");
        let nested = StackGraphScope::new(
            symbol("nested"),
            facts("nested", "stack_graph.scope", vec![SymbolFacet::Namespace]),
            binding("src/lib.rs", 4),
            Some(symbol("workspace")),
        );
        let definition = StackGraphDefinition::new(
            symbol("def.run"),
            facts("run", "stack_graph.definition", vec![SymbolFacet::Value]),
            binding("src/lib.rs", 8),
            symbol("nested"),
        );
        let reference = StackGraphReference::new(
            symbol("ref.run"),
            facts("run", "stack_graph.reference", vec![SymbolFacet::Value]),
            binding("src/main.rs", 16),
            symbol("nested"),
            ReferenceRole::Read,
        );
        let input =
            StackGraphInput::new(vec![workspace, nested], vec![definition], vec![reference]);

        assert_eq!(input.scopes()[1].symbol(), &symbol("nested"));
        assert_eq!(input.scopes()[1].facts().name(), "nested");
        assert_eq!(input.scopes()[1].source().range().start(), 4);
        assert_eq!(input.scopes()[1].parent(), Some(&symbol("workspace")));
        assert_eq!(input.definitions()[0].symbol(), &symbol("def.run"));
        assert_eq!(input.definitions()[0].facts().name(), "run");
        assert_eq!(input.definitions()[0].source().range().start(), 8);
        assert_eq!(input.definitions()[0].scope(), &symbol("nested"));
        assert_eq!(input.references()[0].symbol(), &symbol("ref.run"));
        assert_eq!(input.references()[0].facts().name(), "run");
        assert_eq!(input.references()[0].source().range().start(), 16);
        assert_eq!(input.references()[0].scope(), &symbol("nested"));
        assert_eq!(input.references()[0].role(), ReferenceRole::Read);

        let output = adapter().convert(&input).expect("nested scope publication");
        let nested = &output.publication().contributions()[1];
        assert_eq!(
            nested
                .facts()
                .container_reference()
                .expect("parent Contribution")
                .symbol(),
            &symbol("workspace")
        );
    }

    #[test]
    fn contribution_and_publication_failures_keep_detail() {
        let invalid = StackGraphInput::new(
            vec![StackGraphScope::new(
                symbol("workspace"),
                facts("", "stack_graph.scope", vec![SymbolFacet::Namespace]),
                binding("src/lib.rs", 0),
                None,
            )],
            Vec::new(),
            Vec::new(),
        );
        let error = adapter()
            .convert(&invalid)
            .expect_err("invalid Contribution");
        assert_eq!(error.violation(), StackGraphViolation::InvalidContribution);
        assert!(!error.detail().is_empty());
        assert!(error.to_string().contains("InvalidContribution"));

        let bounded = StackGraphAdapter::new(
            ProviderId::new("stack_graph").expect("provider"),
            ProviderRevision::new(1).expect("publication"),
            SourceRevision::new(2).expect("source"),
            TreeRevision::new(3).expect("tree"),
            PublicationLimits::new(1, 1, 1).expect("limits"),
        );
        let oversized = StackGraphInput::new(
            vec![scope("workspace", "src/lib.rs")],
            vec![definition("def.run", "run", "src/lib.rs")],
            Vec::new(),
        );
        assert_eq!(
            bounded
                .convert(&oversized)
                .expect_err("publication limit")
                .violation(),
            StackGraphViolation::InvalidPublication
        );
    }
}
