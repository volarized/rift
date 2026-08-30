//! Publication: the resolved binding graph becomes one provider publication.
//!
//! [`BindingPublisher::publish`] emits one fact-less Contribution per scope, definition,
//! and reference, each carrying its facts under the `org.rift.binding` extension key. An
//! `is_item` definition adds declaration equivalence so normalization joins it with the
//! syntax provider's declaration, and a resolved reference adds a semantic reference
//! whose targets name the definition Contributions.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{
    Contribution, ContributionBuilder, ContributionError, ContributionKey, ContributionReference,
    DeclarationBinding, EquivalenceEvidence, ExtensionKey, ExtensionValue, Extensions, ProviderId,
    ProviderRevision, ProviderSymbolId, ReferenceRole, SemanticReference, SourceApplicability,
    SourceRange, SourceRevision, SourceUnitId, TreeRevision, encode_path, fault_label,
};
use rift_provider::{
    AdapterError, AdapterPublication, ProviderInputMode, ProviderPublication, PublicationCoverage,
    PublicationError, PublicationLimits,
};
use serde::{Deserialize, Serialize};

use crate::failure::{BindingError, BindingViolation, binding_error};
use crate::graph::{
    BindingGraph, DefinitionId, PathAnchor, Reference, ScopeId, ScopeKind, VisibilitySpelling,
};
use crate::limits::{BindingLimits, ExhaustedLimit};
use crate::link::LinkedGraph;
use crate::resolve::{Resolution, ResolutionSet};

/// Stable identity of the built-in binding fact provider.
pub const BINDING_PROVIDER_ID: &str = "binding";
/// Namespaced extension key every binding Contribution carries its facts under.
pub const BINDING_EXTENSION_KEY: &str = "org.rift.binding";
/// Version of the data shapes published under [`BINDING_EXTENSION_KEY`].
pub const BINDING_EXTENSION_VERSION: u64 = 1;

/// Identity segment of one scope Contribution.
const SCOPE_SEGMENT: &str = "scope";
/// Identity segment of one definition Contribution.
const DEFINITION_SEGMENT: &str = "definition";
/// Identity segment of one reference Contribution.
const REFERENCE_SEGMENT: &str = "reference";

/// One scope kind as spelled in the namespaced data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScopeKindFact {
    Module,
    Block,
    Member,
}

impl From<ScopeKind> for ScopeKindFact {
    fn from(kind: ScopeKind) -> Self {
        match kind {
            ScopeKind::Module => Self::Module,
            ScopeKind::Block => Self::Block,
            ScopeKind::Member => Self::Member,
        }
    }
}

/// One path anchor as spelled in the namespaced data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AnchorFact {
    Crate,
    #[serde(rename = "self")]
    SelfModule,
    Super(u8),
    Lexical,
}

impl From<PathAnchor> for AnchorFact {
    fn from(anchor: PathAnchor) -> Self {
        match anchor {
            PathAnchor::Crate => Self::Crate,
            PathAnchor::SelfModule => Self::SelfModule,
            PathAnchor::Super(levels) => Self::Super(levels),
            PathAnchor::Lexical => Self::Lexical,
        }
    }
}

/// One visibility spelling as the language wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum VisibilityFact {
    #[serde(rename = "pub")]
    Public,
    #[serde(rename = "pub(crate)")]
    Crate,
    #[serde(rename = "pub(super)")]
    Super,
    #[serde(rename = "private")]
    Private,
}

impl From<VisibilitySpelling> for VisibilityFact {
    fn from(spelling: VisibilitySpelling) -> Self {
        match spelling {
            VisibilitySpelling::Public => Self::Public,
            VisibilitySpelling::Crate => Self::Crate,
            VisibilitySpelling::Super => Self::Super,
            VisibilitySpelling::Private => Self::Private,
        }
    }
}

/// One portable reference role as spelled in the namespaced data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RoleFact {
    Definition,
    Read,
    Write,
    Import,
    Call,
    Type,
    Unknown,
}

impl From<ReferenceRole> for RoleFact {
    fn from(role: ReferenceRole) -> Self {
        match role {
            ReferenceRole::Definition => Self::Definition,
            ReferenceRole::Read => Self::Read,
            ReferenceRole::Write => Self::Write,
            ReferenceRole::Import => Self::Import,
            ReferenceRole::Call => Self::Call,
            ReferenceRole::Type => Self::Type,
            ReferenceRole::Unknown => Self::Unknown,
        }
    }
}

/// The data one binding Contribution publishes under [`BINDING_EXTENSION_KEY`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BindingFact {
    Scope {
        scope_kind: ScopeKindFact,
        unit: String,
        range: [u64; 2],
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    Definition {
        name: String,
        unit: String,
        range: [u64; 2],
        scope: String,
        visibility: VisibilityFact,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        declares: Option<String>,
    },
    Reference {
        name_path: Vec<String>,
        anchor: AnchorFact,
        role: RoleFact,
        unit: String,
        range: [u64; 2],
        scope: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exhausted: Option<String>,
    },
}

/// Publishes one binding graph, its links, and its resolutions as provider facts.
///
/// Contributions are fact-less: portable facts stay with the syntax provider, and every
/// binding fact rides under [`BINDING_EXTENSION_KEY`].
#[derive(Debug)]
pub struct BindingPublisher {
    provider: ProviderId,
    revision: ProviderRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    limits: BindingLimits,
    publication_limits: PublicationLimits,
}

impl BindingPublisher {
    /// Prepares one publication for the captured revisions.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::InvalidPublication`] when the built-in provider
    /// identity is refused.
    pub fn new(
        revision: ProviderRevision,
        source_revision: SourceRevision,
        tree_revision: TreeRevision,
        limits: BindingLimits,
        publication_limits: PublicationLimits,
    ) -> Result<Self, BindingError> {
        let provider = ProviderId::new(BINDING_PROVIDER_ID).map_err(|error| {
            binding_error(BindingViolation::InvalidPublication, error.to_string())
        })?;
        Ok(Self {
            provider,
            revision,
            source_revision,
            tree_revision,
            limits,
            publication_limits,
        })
    }

    /// Publishes every scope, definition, and reference of one resolved graph.
    ///
    /// The publication claims [`PublicationCoverage::SourceUnits`] over the graph's unit
    /// table and [`ProviderInputMode::Fact`]. Every loop below iterates one graph table
    /// this method first validates against the publisher's own bounds.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::InvalidPublication`] for a linked graph or resolution
    /// set built from another graph, and for Contributions no publication accepts;
    /// [`BindingViolation::PublicationWork`] when resolution spent the publication work
    /// bound, so the caller keeps its prior publication; [`BindingViolation::GraphLimit`]
    /// when the graph exceeds the publisher's bounds; and
    /// [`BindingViolation::InvalidContribution`] when one binding fact does not form a
    /// valid Contribution.
    pub fn publish(
        &self,
        graph: &BindingGraph,
        linked: &LinkedGraph<'_>,
        resolutions: &ResolutionSet,
    ) -> Result<AdapterPublication, BindingError> {
        self.validate_inputs(graph, linked, resolutions)?;
        let nodes = graph.scopes().len() + graph.definitions().len() + graph.references().len();
        let mut contributions = Vec::with_capacity(nodes);
        self.scope_contributions(graph, &mut contributions)?;
        self.definition_contributions(graph, linked, &mut contributions)?;
        self.reference_contributions(graph, resolutions, &mut contributions)?;
        let publication = ProviderPublication::new(
            self.provider.clone(),
            self.revision,
            contributions,
            self.publication_limits,
        )
        .map_err(|error| publication_refused(&error))?;
        let coverage = PublicationCoverage::SourceUnits(coverage_units(graph));
        AdapterPublication::new(ProviderInputMode::Fact, coverage, publication)
            .map_err(|error| adapter_refused(&error))
    }

    /// Refuses inputs built from another graph, spent work, or an oversized graph.
    fn validate_inputs(
        &self,
        graph: &BindingGraph,
        linked: &LinkedGraph<'_>,
        resolutions: &ResolutionSet,
    ) -> Result<(), BindingError> {
        if !std::ptr::eq(linked.graph(), graph) {
            return Err(binding_error(
                BindingViolation::InvalidPublication,
                "the linked graph borrows another graph; link and publish one BindingGraph value",
            ));
        }
        if resolutions.resolutions().len() != graph.references().len() {
            let detail = format!(
                "expected one resolution per reference: {} references, {} resolutions",
                graph.references().len(),
                resolutions.resolutions().len()
            );
            return Err(binding_error(BindingViolation::InvalidPublication, detail));
        }
        if let Some(limit) = resolutions.exhausted() {
            let detail = format!(
                "resolution spent the {} bound; keep the prior publication or raise the bound",
                fault_label(&limit)
            );
            return Err(binding_error(
                BindingViolation::PublicationWork(limit),
                detail,
            ));
        }
        self.validate_counts(graph)
    }

    /// Revalidates graph sizes, so a graph built under wider bounds cannot pass through.
    fn validate_counts(&self, graph: &BindingGraph) -> Result<(), BindingError> {
        let nodes = graph.scopes().len() + graph.definitions().len() + graph.references().len();
        if nodes > self.limits.graph_nodes_max() {
            let detail = format!(
                "{nodes} graph nodes, bound {}",
                self.limits.graph_nodes_max()
            );
            let violation = BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes);
            return Err(binding_error(violation, detail));
        }
        if graph.links().len() > self.limits.graph_links_max() {
            let detail = format!(
                "{} graph links, bound {}",
                graph.links().len(),
                self.limits.graph_links_max()
            );
            let violation = BindingViolation::GraphLimit(ExhaustedLimit::GraphLinks);
            return Err(binding_error(violation, detail));
        }
        Ok(())
    }

    /// Publishes one Contribution per scope; the validated scope table bounds the loop.
    fn scope_contributions(
        &self,
        graph: &BindingGraph,
        contributions: &mut Vec<Contribution>,
    ) -> Result<(), BindingError> {
        for (scope, id) in graph.scopes().iter().zip(graph.scope_ids()) {
            let unit = graph.unit(scope.unit());
            let fact = BindingFact::Scope {
                scope_kind: scope.kind().into(),
                unit: unit.source().key().as_str().to_owned(),
                range: [scope.range().start(), scope.range().end()],
                parent: scope
                    .parent()
                    .map(|parent| scope_symbol_text(graph, parent)),
            };
            let symbol = scope_symbol_text(graph, id);
            let builder = self.start_contribution(symbol, unit.origin().clone(), &fact)?;
            contributions.push(
                builder
                    .build()
                    .map_err(|error| contribution_refused(&error))?,
            );
        }
        Ok(())
    }

    /// Publishes one Contribution per definition; the validated table bounds the loop.
    ///
    /// An `is_item` definition carries declaration equivalence over its item range, so
    /// normalization associates it with the syntax provider's declaration.
    fn definition_contributions(
        &self,
        graph: &BindingGraph,
        linked: &LinkedGraph<'_>,
        contributions: &mut Vec<Contribution>,
    ) -> Result<(), BindingError> {
        for (definition, id) in graph.definitions().iter().zip(graph.definition_ids()) {
            let unit = graph.unit(graph.scope(definition.scope()).unit());
            let fact = BindingFact::Definition {
                name: definition.name().as_str().to_owned(),
                unit: unit.source().key().as_str().to_owned(),
                range: [definition.range().start(), definition.range().end()],
                scope: scope_symbol_text(graph, definition.scope()),
                visibility: definition.visibility().into(),
                declares: linked
                    .declared_scope(id)
                    .map(|scope| scope_symbol_text(graph, scope)),
            };
            let symbol = definition_symbol_text(graph, id);
            let mut builder = self.start_contribution(symbol, unit.origin().clone(), &fact)?;
            if definition.is_item() {
                let declaration =
                    DeclarationBinding::new(unit.source().clone(), definition.range(), None);
                builder = builder.equivalence(vec![EquivalenceEvidence::Declaration(declaration)]);
            }
            contributions.push(
                builder
                    .build()
                    .map_err(|error| contribution_refused(&error))?,
            );
        }
        Ok(())
    }

    /// Publishes one Contribution per reference; the validated table bounds the loop.
    ///
    /// A resolved reference carries its targets as a semantic reference; an exhausted or
    /// unresolved one publishes no targets, and an exhausted one names its spent bound in
    /// the namespaced data.
    fn reference_contributions(
        &self,
        graph: &BindingGraph,
        resolutions: &ResolutionSet,
        contributions: &mut Vec<Contribution>,
    ) -> Result<(), BindingError> {
        for (reference, id) in graph.references().iter().zip(graph.reference_ids()) {
            let resolution = resolutions.resolution(id);
            let unit = graph.unit(graph.scope(reference.scope()).unit());
            let fact = BindingFact::Reference {
                name_path: reference
                    .path()
                    .segments()
                    .iter()
                    .map(|name| name.as_str().to_owned())
                    .collect(),
                anchor: reference.anchor().into(),
                role: reference.role().into(),
                unit: unit.source().key().as_str().to_owned(),
                range: [reference.range().start(), reference.range().end()],
                scope: scope_symbol_text(graph, reference.scope()),
                exhausted: resolution.exhausted().map(|limit| fault_label(&limit)),
            };
            let symbol = symbol_text(REFERENCE_SEGMENT, unit.source(), reference.range());
            let mut builder = self.start_contribution(symbol, unit.origin().clone(), &fact)?;
            if !resolution.targets().is_empty() {
                let semantic =
                    self.semantic_reference(graph, unit.source(), reference, resolution)?;
                builder = builder.references(vec![semantic]);
            }
            contributions.push(
                builder
                    .build()
                    .map_err(|error| contribution_refused(&error))?,
            );
        }
        Ok(())
    }

    /// One semantic reference whose targets name this publication's definition symbols.
    fn semantic_reference(
        &self,
        graph: &BindingGraph,
        unit: &SourceUnitId,
        reference: &Reference,
        resolution: &Resolution,
    ) -> Result<SemanticReference, BindingError> {
        let mut targets = Vec::with_capacity(resolution.targets().len());
        for &definition in resolution.targets() {
            let symbol = provider_symbol(definition_symbol_text(graph, definition))?;
            targets.push(ContributionReference::new(self.provider.clone(), symbol));
        }
        let source = DeclarationBinding::new(unit.clone(), reference.range(), None);
        SemanticReference::new(source, reference.role(), targets)
            .map_err(|error| contribution_refused(&error))
    }

    /// Starts one fact-less Contribution carrying `fact` under the binding extension key.
    fn start_contribution(
        &self,
        symbol: String,
        origin: rift_core::ContributionOrigin,
        fact: &BindingFact,
    ) -> Result<ContributionBuilder, BindingError> {
        let key = ContributionKey::new(
            self.provider.clone(),
            self.revision,
            provider_symbol(symbol)?,
        );
        let builder = Contribution::fact_builder(key, self.applicability(), origin)
            .namespaced(namespaced(fact)?);
        Ok(builder)
    }

    const fn applicability(&self) -> SourceApplicability {
        SourceApplicability::Exact {
            source_revision: self.source_revision,
            tree_revision: self.tree_revision,
        }
    }
}

/// Every unit exactly once, in unit-table order; the validated table bounds the loop.
fn coverage_units(graph: &BindingGraph) -> Vec<SourceUnitId> {
    let mut seen = BTreeSet::new();
    let mut units = Vec::with_capacity(graph.units().len());
    for unit in graph.units() {
        if seen.insert(unit.source().clone()) {
            units.push(unit.source().clone());
        }
    }
    units
}

/// Mints one provider-local symbol spelling: `rift://binding/<segment>/<key>@<start>-<end>`.
fn symbol_text(segment: &str, unit: &SourceUnitId, range: SourceRange) -> String {
    format!(
        "rift://binding/{segment}/{}@{}-{}",
        encode_path(unit.key().as_str()),
        range.start(),
        range.end()
    )
}

fn scope_symbol_text(graph: &BindingGraph, id: ScopeId) -> String {
    let scope = graph.scope(id);
    symbol_text(
        SCOPE_SEGMENT,
        graph.unit(scope.unit()).source(),
        scope.range(),
    )
}

fn definition_symbol_text(graph: &BindingGraph, id: DefinitionId) -> String {
    let definition = graph.definition(id);
    let unit = graph.unit(graph.scope(definition.scope()).unit());
    symbol_text(DEFINITION_SEGMENT, unit.source(), definition.range())
}

fn provider_symbol(text: String) -> Result<ProviderSymbolId, BindingError> {
    ProviderSymbolId::new(text)
        .map_err(|error| binding_error(BindingViolation::InvalidContribution, error.to_string()))
}

/// Encodes one binding fact under the versioned binding extension key.
fn namespaced(fact: &BindingFact) -> Result<Extensions, BindingError> {
    let data = serde_json::to_value(fact)
        .map_err(|error| binding_error(BindingViolation::InvalidContribution, error.to_string()))?;
    let value = ExtensionValue {
        version: BINDING_EXTENSION_VERSION,
        data,
    };
    let mut entries = BTreeMap::new();
    entries.insert(ExtensionKey(BINDING_EXTENSION_KEY.to_owned()), value);
    Ok(Extensions(entries))
}

fn contribution_refused(error: &ContributionError) -> BindingError {
    binding_error(BindingViolation::InvalidContribution, error.to_string())
}

fn publication_refused(error: &PublicationError) -> BindingError {
    binding_error(BindingViolation::InvalidPublication, error.to_string())
}

fn adapter_refused(error: &AdapterError) -> BindingError {
    binding_error(BindingViolation::InvalidPublication, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rift_core::{
        CONTRIBUTION_FACTS_MAX, Contribution, ContributionKey, ContributionReference,
        DeclarationBinding, EquivalenceEvidence, ExactKind, ExtensionKey, IndexRevision, Language,
        PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId, ReferenceRole,
        SourceApplicability, SourceRevision, SymbolId, SymbolResolution, TreeRevision,
    };
    use rift_provider::{
        AdapterPublication, NormalizedGraph, NormalizedTarget, Normalizer, ProviderInputMode,
        ProviderPublication, PublicationCoverage, PublicationLimits, PublicationSet,
    };

    use super::{
        AnchorFact, BINDING_EXTENSION_KEY, BINDING_EXTENSION_VERSION, BINDING_PROVIDER_ID,
        BindingFact, BindingPublisher, RoleFact, ScopeKindFact, VisibilityFact,
    };
    use crate::failure::BindingViolation;
    use crate::fixture::{self, Fixture};
    use crate::graph::{
        BindingGraph, Definition, DefinitionOrder, PathAnchor, Reference, VisibilitySpelling,
    };
    use crate::limits::{BindingLimits, ExhaustedLimit};
    use crate::link::LinkedGraph;
    use crate::resolve::{NeverCancelled, ResolutionSet, resolve_all};

    const RUN_UNIT: &str = "src/run.rs";
    const SYNTAX_RUN_IDENTITY: &str = "rift://symbol/rust/src/run.rs/run";
    const BINDING_RUN_DEFINITION: &str = "rift://binding/definition/src/run.rs@10-30";
    const BINDING_HELPER_DEFINITION: &str = "rift://binding/definition/src/run.rs@40-60";

    fn publisher() -> BindingPublisher {
        publisher_with(BindingLimits::default())
    }

    fn publisher_with(limits: BindingLimits) -> BindingPublisher {
        BindingPublisher::new(
            ProviderRevision::new(1).expect("revision"),
            SourceRevision::new(1).expect("source revision"),
            TreeRevision::new(1).expect("tree revision"),
            limits,
            PublicationLimits::default(),
        )
        .expect("publisher")
    }

    fn linked_and_resolved<'graph>(
        graph: &'graph BindingGraph,
        limits: &BindingLimits,
    ) -> (LinkedGraph<'graph>, ResolutionSet) {
        let linked = LinkedGraph::link(graph, limits).expect("graph links");
        let resolutions =
            resolve_all(&linked, limits, &NeverCancelled).expect("resolution completes");
        (linked, resolutions)
    }

    fn published(graph: &BindingGraph, limits: BindingLimits) -> AdapterPublication {
        let (linked, resolutions) = linked_and_resolved(graph, &limits);
        publisher_with(limits)
            .publish(graph, &linked, &resolutions)
            .expect("publication accepted")
    }

    fn contribution_with_symbol<'publication>(
        publication: &'publication ProviderPublication,
        symbol: &str,
    ) -> &'publication Contribution {
        publication
            .contributions()
            .iter()
            .find(|contribution| contribution.key().reference().symbol().as_str() == symbol)
            .expect("contribution present")
    }

    fn fact_of(contribution: &Contribution) -> BindingFact {
        let key = ExtensionKey(BINDING_EXTENSION_KEY.to_owned());
        let value = contribution
            .namespaced()
            .0
            .get(&key)
            .expect("binding extension present");
        assert_eq!(value.version, BINDING_EXTENSION_VERSION);
        serde_json::from_value(value.data.clone()).expect("binding fact decodes")
    }

    #[test]
    fn test_provider_symbol_id_binding_spelling_accepted() {
        for spelling in [
            "rift://binding/scope/src/lib.rs@0-10",
            "rift://binding/definition/src/lib.rs@10-30",
            "rift://binding/reference/src/lib.rs@40-41",
        ] {
            let symbol = ProviderSymbolId::new(spelling).expect("binding spelling accepted");
            assert_eq!(symbol.as_str(), spelling);
        }
    }

    #[test]
    fn test_publisher_cross_file_reference_resolves_through_module_and_import() {
        let mut fixture = Fixture::new();
        let lib = fixture.unit("src/lib.rs");
        let run = fixture.unit(RUN_UNIT);
        let lib_scope = fixture.module(lib, None, 0, 100);
        let run_scope = fixture.module(run, None, 0, 50);
        let module_definition = fixture.item(lib_scope, "run", 1, 9, VisibilitySpelling::Private);
        fixture.module_link(lib_scope, module_definition, RUN_UNIT);
        fixture.item(run_scope, "target", 5, 15, VisibilitySpelling::Public);
        let anchor = PathAnchor::SelfModule;
        fixture.import(
            lib_scope,
            Some("target"),
            anchor,
            "run::target",
            0,
            VisibilitySpelling::Private,
        );
        fixture.reference(lib_scope, 60, PathAnchor::Lexical, "target");
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let reference = contribution_with_symbol(
            output.publication(),
            "rift://binding/reference/src/lib.rs@60-61",
        );
        assert_eq!(reference.references().len(), 1);
        let semantic = &reference.references()[0];
        assert_eq!(semantic.role(), ReferenceRole::Read);
        assert_eq!(semantic.source().unit(), &fixture::source("src/lib.rs"));
        let targets: Vec<&str> = semantic
            .targets()
            .iter()
            .map(|target| target.symbol().as_str())
            .collect();
        assert_eq!(targets, ["rift://binding/definition/src/run.rs@5-15"]);
    }

    #[test]
    fn test_publisher_unrelated_same_name_definitions_stay_untargeted() {
        let mut fixture = Fixture::new();
        let one = fixture.unit("src/one.rs");
        let two = fixture.unit("src/two.rs");
        let three = fixture.unit("src/three.rs");
        let one_scope = fixture.module(one, None, 0, 50);
        let two_scope = fixture.module(two, None, 0, 50);
        let three_scope = fixture.module(three, None, 0, 50);
        fixture.item(one_scope, "run", 1, 9, VisibilitySpelling::Public);
        fixture.item(two_scope, "run", 1, 9, VisibilitySpelling::Public);
        fixture.reference(three_scope, 10, PathAnchor::Lexical, "run");
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let reference = contribution_with_symbol(
            output.publication(),
            "rift://binding/reference/src/three.rs@10-11",
        );
        assert!(
            reference.references().is_empty(),
            "no root shortcut joins unrelated units"
        );
        let BindingFact::Reference {
            exhausted,
            name_path,
            ..
        } = fact_of(reference)
        else {
            panic!("reference fact expected");
        };
        assert_eq!(exhausted, None);
        assert_eq!(name_path, ["run"]);
    }

    #[test]
    fn test_publisher_ambiguous_targets_publish_in_stable_order() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.item(module, "run", 1, 9, VisibilitySpelling::Public);
        fixture.item(module, "run", 11, 19, VisibilitySpelling::Public);
        fixture.reference(module, 30, PathAnchor::Lexical, "run");
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let reference = contribution_with_symbol(
            output.publication(),
            "rift://binding/reference/src/lib.rs@30-31",
        );
        let targets: Vec<&str> = reference.references()[0]
            .targets()
            .iter()
            .map(|target| target.symbol().as_str())
            .collect();
        assert_eq!(
            targets,
            [
                "rift://binding/definition/src/lib.rs@1-9",
                "rift://binding/definition/src/lib.rs@11-19",
            ]
        );
    }

    #[test]
    fn test_publisher_unresolved_reference_publishes_without_references() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.reference(module, 10, PathAnchor::Lexical, "missing");
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let reference = contribution_with_symbol(
            output.publication(),
            "rift://binding/reference/src/lib.rs@10-11",
        );
        assert!(reference.references().is_empty());
        let BindingFact::Reference { exhausted, .. } = fact_of(reference) else {
            panic!("reference fact expected");
        };
        assert_eq!(exhausted, None);
    }

    #[test]
    fn test_publisher_distinct_facts_mint_distinct_symbols() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.block(unit, Some(module), 40, 90);
        fixture.item(module, "run", 1, 9, VisibilitySpelling::Public);
        fixture.item(module, "walk", 11, 19, VisibilitySpelling::Public);
        fixture.reference(module, 30, PathAnchor::Lexical, "run");
        fixture.reference(module, 32, PathAnchor::Lexical, "walk");
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let symbols: std::collections::BTreeSet<&str> = output
            .publication()
            .contributions()
            .iter()
            .map(|contribution| contribution.key().reference().symbol().as_str())
            .collect();
        assert_eq!(output.publication().contributions().len(), 6);
        assert_eq!(symbols.len(), 6, "every fact mints its own provider symbol");
    }

    #[test]
    fn test_publisher_exhausted_reference_publishes_exhausted_label() {
        let limits = BindingLimits::builder()
            .reference_work_max(1)
            .build()
            .expect("limits");
        let mut fixture = Fixture::with_limits(limits);
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        let block = fixture.block(unit, Some(module), 10, 90);
        fixture.reference(block, 50, PathAnchor::Lexical, "missing");
        let graph = fixture.build();
        let output = published(&graph, limits);
        let reference = contribution_with_symbol(
            output.publication(),
            "rift://binding/reference/src/lib.rs@50-51",
        );
        assert!(
            reference.references().is_empty(),
            "an exhausted reference publishes no targets"
        );
        let BindingFact::Reference { exhausted, .. } = fact_of(reference) else {
            panic!("reference fact expected");
        };
        assert_eq!(exhausted.as_deref(), Some("reference_work"));
    }

    #[test]
    fn test_publisher_publication_work_exhaustion_refused() {
        let limits = BindingLimits::builder()
            .publication_work_max(1)
            .build()
            .expect("limits");
        let mut fixture = Fixture::with_limits(limits);
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        let block = fixture.block(unit, Some(module), 10, 90);
        fixture.reference(block, 50, PathAnchor::Lexical, "missing");
        let graph = fixture.build();
        let (linked, resolutions) = linked_and_resolved(&graph, &limits);
        assert_eq!(
            resolutions.exhausted(),
            Some(ExhaustedLimit::PublicationWork)
        );
        let error = publisher_with(limits)
            .publish(&graph, &linked, &resolutions)
            .expect_err("spent publication work refuses the publication");
        assert_eq!(
            error.fault().violation(),
            BindingViolation::PublicationWork(ExhaustedLimit::PublicationWork)
        );
    }

    #[test]
    fn test_publisher_coverage_lists_every_unit_once() {
        let mut fixture = Fixture::new();
        let lib = fixture.unit("src/lib.rs");
        let run = fixture.unit(RUN_UNIT);
        fixture.module(lib, None, 0, 100);
        fixture.module(run, None, 0, 50);
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        assert_eq!(output.mode(), ProviderInputMode::Fact);
        let expected = vec![fixture::source("src/lib.rs"), fixture::source(RUN_UNIT)];
        assert_eq!(
            output.coverage(),
            &PublicationCoverage::SourceUnits(expected)
        );
    }

    #[test]
    fn test_publisher_empty_graph_refused() {
        let graph = Fixture::new().build();
        let limits = BindingLimits::default();
        let (linked, resolutions) = linked_and_resolved(&graph, &limits);
        let error = publisher()
            .publish(&graph, &linked, &resolutions)
            .expect_err("a publication with no source-unit coverage is refused");
        assert_eq!(
            error.fault().violation(),
            BindingViolation::InvalidPublication
        );
    }

    #[test]
    fn test_publisher_duplicate_scope_spelling_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        fixture.module(unit, None, 0, 100);
        fixture.module(unit, None, 0, 100);
        let graph = fixture.build();
        let limits = BindingLimits::default();
        let (linked, resolutions) = linked_and_resolved(&graph, &limits);
        let error = publisher()
            .publish(&graph, &linked, &resolutions)
            .expect_err("two scopes over one range spell one symbol");
        assert_eq!(
            error.fault().violation(),
            BindingViolation::InvalidPublication
        );
    }

    #[test]
    fn test_publisher_foreign_linked_graph_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        fixture.module(unit, None, 0, 100);
        let graph = fixture.build();
        let copy = graph.clone();
        let limits = BindingLimits::default();
        let (linked, resolutions) = linked_and_resolved(&graph, &limits);
        let error = publisher()
            .publish(&copy, &linked, &resolutions)
            .expect_err("a linked graph borrowing another graph value is refused");
        assert_eq!(
            error.fault().violation(),
            BindingViolation::InvalidPublication
        );
    }

    #[test]
    fn test_publisher_resolution_count_mismatch_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.reference(module, 10, PathAnchor::Lexical, "missing");
        let graph = fixture.build();
        let mut other = Fixture::new();
        let other_unit = other.unit("src/lib.rs");
        other.module(other_unit, None, 0, 100);
        let other_graph = other.build();
        let limits = BindingLimits::default();
        let (linked, _) = linked_and_resolved(&graph, &limits);
        let (_, other_resolutions) = linked_and_resolved(&other_graph, &limits);
        let error = publisher()
            .publish(&graph, &linked, &other_resolutions)
            .expect_err("a resolution set from another graph is refused");
        assert_eq!(
            error.fault().violation(),
            BindingViolation::InvalidPublication
        );
    }

    #[test]
    fn test_publisher_graph_beyond_limits_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.block(unit, Some(module), 10, 90);
        let anchor = PathAnchor::SelfModule;
        fixture.import(
            module,
            Some("one"),
            anchor,
            "run::one",
            0,
            VisibilitySpelling::Private,
        );
        fixture.import(
            module,
            Some("two"),
            anchor,
            "run::two",
            0,
            VisibilitySpelling::Private,
        );
        let graph = fixture.build();
        let defaults = BindingLimits::default();
        let (linked, resolutions) = linked_and_resolved(&graph, &defaults);
        let cases = [
            (
                BindingLimits::builder()
                    .graph_nodes_max(1)
                    .build()
                    .expect("limits"),
                ExhaustedLimit::GraphNodes,
            ),
            (
                BindingLimits::builder()
                    .graph_links_max(1)
                    .build()
                    .expect("limits"),
                ExhaustedLimit::GraphLinks,
            ),
        ];
        for (limits, expected) in cases {
            let error = publisher_with(limits)
                .publish(&graph, &linked, &resolutions)
                .expect_err("a graph beyond the publisher's bounds is refused");
            assert_eq!(
                error.fault().violation(),
                BindingViolation::GraphLimit(expected)
            );
        }
    }

    #[test]
    fn test_publisher_targets_beyond_contribution_bound_refused() {
        let limits = BindingLimits::builder()
            .reference_targets_max(CONTRIBUTION_FACTS_MAX + 1)
            .build()
            .expect("limits");
        let mut fixture = Fixture::with_limits(limits);
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 4096);
        for index in 0..=CONTRIBUTION_FACTS_MAX {
            let start = 10 + 2 * (index as u64);
            fixture.item(module, "run", start, start + 1, VisibilitySpelling::Public);
        }
        fixture.reference(module, 4000, PathAnchor::Lexical, "run");
        let graph = fixture.build();
        let (linked, resolutions) = linked_and_resolved(&graph, &limits);
        let error = publisher_with(limits)
            .publish(&graph, &linked, &resolutions)
            .expect_err("more targets than one Contribution carries are refused");
        assert_eq!(
            error.fault().violation(),
            BindingViolation::InvalidContribution
        );
    }

    fn binding_publication() -> ProviderPublication {
        let mut fixture = Fixture::new();
        let unit = fixture.unit(RUN_UNIT);
        let module = fixture.module(unit, None, 0, 100);
        let item = Definition::new(
            module,
            fixture::name("run"),
            fixture::range(10, 30),
            fixture::kind(),
            DefinitionOrder::Item,
            VisibilitySpelling::Public,
        )
        .item();
        fixture
            .builder
            .definition(item)
            .expect("item definition accepted");
        fixture.item(module, "helper", 40, 60, VisibilitySpelling::Private);
        fixture.reference(module, 70, PathAnchor::Lexical, "run");
        let graph = fixture.build();
        published(&graph, BindingLimits::default()).into_publication()
    }

    fn syntax_publication() -> ProviderPublication {
        let provider = ProviderId::new("syntax").expect("provider");
        let revision = ProviderRevision::new(1).expect("revision");
        let identity = SymbolId::new(SYNTAX_RUN_IDENTITY).expect("identity");
        let facts = PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            "run",
            "run",
            ExactKind("rust.function".to_owned()),
        );
        let source =
            DeclarationBinding::new(fixture::source(RUN_UNIT), fixture::range(10, 30), None);
        let contribution = Contribution::builder(
            ContributionKey::new(
                provider.clone(),
                revision,
                ProviderSymbolId::new(SYNTAX_RUN_IDENTITY).expect("symbol"),
            ),
            SourceApplicability::Exact {
                source_revision: SourceRevision::new(1).expect("source revision"),
                tree_revision: TreeRevision::new(1).expect("tree revision"),
            },
            facts,
            fixture::origin(),
        )
        .source(source)
        .identity_anchor(identity)
        .build()
        .expect("syntax contribution");
        ProviderPublication::new(
            provider,
            revision,
            vec![contribution],
            PublicationLimits::default(),
        )
        .expect("syntax publication")
    }

    fn normalized() -> NormalizedGraph {
        let mut set = PublicationSet::empty(PublicationLimits::default());
        set = set.replaced(syntax_publication()).expect("syntax accepted");
        set = set
            .replaced(binding_publication())
            .expect("binding accepted");
        let publications = Arc::new(set);
        Normalizer::normalize(
            IndexRevision::new(1).expect("index revision"),
            SourceRevision::new(1).expect("source revision"),
            TreeRevision::new(1).expect("tree revision"),
            &publications,
            None,
        )
        .expect("normalized graph")
    }

    fn reference_of(provider: &str, symbol: &str) -> ContributionReference {
        ContributionReference::new(
            ProviderId::new(provider).expect("provider"),
            ProviderSymbolId::new(symbol).expect("symbol"),
        )
    }

    #[test]
    fn test_publisher_item_definition_joins_syntax_record() {
        let graph = normalized();
        let binding_record = graph
            .record_for(&reference_of(BINDING_PROVIDER_ID, BINDING_RUN_DEFINITION))
            .expect("binding record");
        let syntax_record = graph
            .record_for(&reference_of("syntax", SYNTAX_RUN_IDENTITY))
            .expect("syntax record");
        assert_eq!(
            binding_record, syntax_record,
            "declaration equivalence joins one record"
        );
        assert_eq!(
            binding_record.identity().map(SymbolId::as_str),
            Some(SYNTAX_RUN_IDENTITY)
        );
    }

    #[test]
    fn test_publisher_reference_target_becomes_syntax_symbol() {
        let graph = normalized();
        let reference = graph
            .references()
            .iter()
            .find(|reference| {
                reference.source().reference().provider().as_str() == BINDING_PROVIDER_ID
            })
            .expect("binding reference");
        let expected =
            NormalizedTarget::Symbol(SymbolId::new(SYNTAX_RUN_IDENTITY).expect("identity"));
        assert_eq!(reference.targets(), [expected]);
        assert_eq!(reference.role(), ReferenceRole::Read);
    }

    #[test]
    fn test_publisher_non_item_definition_stays_unresolved() {
        let graph = normalized();
        let record = graph
            .record_for(&reference_of(
                BINDING_PROVIDER_ID,
                BINDING_HELPER_DEFINITION,
            ))
            .expect("helper record");
        assert_eq!(record.resolution(), SymbolResolution::Unresolved);
        assert_eq!(
            record.identity(),
            None,
            "a non-item definition mints no SymbolId"
        );
    }

    #[test]
    fn test_binding_fact_round_trips_through_serde() {
        let mut facts = vec![
            BindingFact::Scope {
                scope_kind: ScopeKindFact::Module,
                unit: RUN_UNIT.to_owned(),
                range: [0, 100],
                parent: None,
            },
            BindingFact::Scope {
                scope_kind: ScopeKindFact::Block,
                unit: RUN_UNIT.to_owned(),
                range: [10, 90],
                parent: Some("rift://binding/scope/src/run.rs@0-100".to_owned()),
            },
            BindingFact::Scope {
                scope_kind: ScopeKindFact::Member,
                unit: RUN_UNIT.to_owned(),
                range: [20, 80],
                parent: None,
            },
        ];
        let visibilities = [
            VisibilityFact::Public,
            VisibilityFact::Crate,
            VisibilityFact::Super,
            VisibilityFact::Private,
        ];
        for (index, visibility) in visibilities.into_iter().enumerate() {
            facts.push(BindingFact::Definition {
                name: "run".to_owned(),
                unit: RUN_UNIT.to_owned(),
                range: [10, 30],
                scope: "rift://binding/scope/src/run.rs@0-100".to_owned(),
                visibility,
                declares: (index == 0).then(|| "rift://binding/scope/src/run.rs@20-80".to_owned()),
            });
        }
        let anchors = [
            AnchorFact::Crate,
            AnchorFact::SelfModule,
            AnchorFact::Super(2),
            AnchorFact::Lexical,
        ];
        let roles = [
            RoleFact::Definition,
            RoleFact::Read,
            RoleFact::Write,
            RoleFact::Import,
            RoleFact::Call,
            RoleFact::Type,
            RoleFact::Unknown,
        ];
        for (index, role) in roles.into_iter().enumerate() {
            facts.push(BindingFact::Reference {
                name_path: vec!["run".to_owned()],
                anchor: anchors[index % anchors.len()],
                role,
                unit: RUN_UNIT.to_owned(),
                range: [70, 71],
                scope: "rift://binding/scope/src/run.rs@0-100".to_owned(),
                exhausted: (index == 0).then(|| "reference_work".to_owned()),
            });
        }
        for fact in facts {
            let value = serde_json::to_value(&fact).expect("fact encodes");
            let decoded: BindingFact = serde_json::from_value(value).expect("fact decodes");
            assert_eq!(decoded, fact);
        }
    }

    #[test]
    fn test_binding_fact_spellings_stay_stable() {
        let definition = BindingFact::Definition {
            name: "run".to_owned(),
            unit: RUN_UNIT.to_owned(),
            range: [10, 30],
            scope: "rift://binding/scope/src/run.rs@0-100".to_owned(),
            visibility: VisibilityFact::Crate,
            declares: None,
        };
        let value = serde_json::to_value(&definition).expect("definition encodes");
        assert_eq!(value["kind"], "definition");
        assert_eq!(value["visibility"], "pub(crate)");
        assert_eq!(
            value.get("declares"),
            None,
            "an absent option stays off the wire"
        );
        let reference = BindingFact::Reference {
            name_path: vec!["run".to_owned()],
            anchor: AnchorFact::SelfModule,
            role: RoleFact::Call,
            unit: RUN_UNIT.to_owned(),
            range: [70, 71],
            scope: "rift://binding/scope/src/run.rs@0-100".to_owned(),
            exhausted: None,
        };
        let value = serde_json::to_value(&reference).expect("reference encodes");
        assert_eq!(value["kind"], "reference");
        assert_eq!(value["anchor"], "self");
        assert_eq!(value["role"], "call");
        assert_eq!(value.get("exhausted"), None);
        let levels = serde_json::to_value(AnchorFact::Super(2)).expect("anchor encodes");
        assert_eq!(levels, serde_json::json!({ "super": 2 }));
    }

    #[test]
    fn test_publisher_extension_key_and_version_stay_stable() {
        assert_eq!(BINDING_PROVIDER_ID, "binding");
        assert_eq!(BINDING_EXTENSION_KEY, "org.rift.binding");
        assert_eq!(BINDING_EXTENSION_VERSION, 1);
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        fixture.module(unit, None, 0, 100);
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let contribution = &output.publication().contributions()[0];
        assert_eq!(
            output.publication().provider().as_str(),
            BINDING_PROVIDER_ID
        );
        let keys: Vec<&str> = contribution
            .namespaced()
            .0
            .keys()
            .map(|key| key.0.as_str())
            .collect();
        assert_eq!(keys, [BINDING_EXTENSION_KEY]);
    }

    #[test]
    fn test_publisher_scope_contribution_carries_parent_and_unit() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.block(unit, Some(module), 10, 90);
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let block = contribution_with_symbol(
            output.publication(),
            "rift://binding/scope/src/lib.rs@10-90",
        );
        assert!(
            block.facts().is_none(),
            "binding Contributions carry no portable facts"
        );
        assert!(
            block.source().is_none(),
            "binding Contributions bind no declaration"
        );
        let BindingFact::Scope {
            scope_kind,
            unit,
            range,
            parent,
        } = fact_of(block)
        else {
            panic!("scope fact expected");
        };
        assert_eq!(scope_kind, ScopeKindFact::Block);
        assert_eq!(unit, "src/lib.rs");
        assert_eq!(range, [10, 90]);
        assert_eq!(
            parent.as_deref(),
            Some("rift://binding/scope/src/lib.rs@0-100")
        );
    }

    #[test]
    fn test_publisher_definition_contribution_carries_declares_and_equivalence() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit(RUN_UNIT);
        let module = fixture.module(unit, None, 0, 100);
        let inner = fixture.module(unit, Some(module), 20, 80);
        fixture.declaring_item(module, "inner", 10, 80, VisibilitySpelling::Crate, inner);
        let item = Definition::new(
            module,
            fixture::name("run"),
            fixture::range(10, 30),
            fixture::kind(),
            DefinitionOrder::Item,
            VisibilitySpelling::Public,
        )
        .item();
        fixture
            .builder
            .definition(item)
            .expect("item definition accepted");
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let declaring = contribution_with_symbol(
            output.publication(),
            "rift://binding/definition/src/run.rs@10-80",
        );
        let BindingFact::Definition {
            visibility,
            declares,
            scope,
            ..
        } = fact_of(declaring)
        else {
            panic!("definition fact expected");
        };
        assert_eq!(visibility, VisibilityFact::Crate);
        assert_eq!(
            declares.as_deref(),
            Some("rift://binding/scope/src/run.rs@20-80")
        );
        assert_eq!(scope, "rift://binding/scope/src/run.rs@0-100");
        assert!(
            declaring.equivalence().is_empty(),
            "only is_item definitions carry equivalence"
        );
        let item = contribution_with_symbol(output.publication(), BINDING_RUN_DEFINITION);
        let declaration =
            DeclarationBinding::new(fixture::source(RUN_UNIT), fixture::range(10, 30), None);
        assert_eq!(
            item.equivalence(),
            [EquivalenceEvidence::Declaration(declaration)]
        );
    }

    #[test]
    fn test_publisher_member_scope_publishes_member_kind() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.member(unit, Some(module), 20, 40);
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let member = contribution_with_symbol(
            output.publication(),
            "rift://binding/scope/src/lib.rs@20-40",
        );
        let BindingFact::Scope { scope_kind, .. } = fact_of(member) else {
            panic!("scope fact expected");
        };
        assert_eq!(scope_kind, ScopeKindFact::Member);
    }

    #[test]
    fn test_publisher_reference_anchors_publish_as_spelled() {
        let cases = [
            (10, PathAnchor::Crate, AnchorFact::Crate),
            (20, PathAnchor::SelfModule, AnchorFact::SelfModule),
            (30, PathAnchor::Super(2), AnchorFact::Super(2)),
        ];
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        for (start, anchor, _) in cases {
            fixture.reference(module, start, anchor, "run");
        }
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        for (start, _, expected) in cases {
            let symbol = format!("rift://binding/reference/src/lib.rs@{start}-{}", start + 1);
            let reference = contribution_with_symbol(output.publication(), &symbol);
            let BindingFact::Reference { anchor, .. } = fact_of(reference) else {
                panic!("reference fact expected");
            };
            assert_eq!(anchor, expected, "anchor fact for {symbol}");
        }
    }

    #[test]
    fn test_publisher_super_visibility_publishes_super_fact() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        fixture.item(module, "run", 1, 9, VisibilitySpelling::Super);
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        let definition = contribution_with_symbol(
            output.publication(),
            "rift://binding/definition/src/lib.rs@1-9",
        );
        let BindingFact::Definition { visibility, .. } = fact_of(definition) else {
            panic!("definition fact expected");
        };
        assert_eq!(visibility, VisibilityFact::Super);
    }

    #[test]
    fn test_publisher_reference_roles_publish_as_spelled() {
        let cases = [
            (10, ReferenceRole::Definition, RoleFact::Definition),
            (20, ReferenceRole::Read, RoleFact::Read),
            (30, ReferenceRole::Write, RoleFact::Write),
            (40, ReferenceRole::Import, RoleFact::Import),
            (50, ReferenceRole::Call, RoleFact::Call),
            (60, ReferenceRole::Type, RoleFact::Type),
            (70, ReferenceRole::Unknown, RoleFact::Unknown),
        ];
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let module = fixture.module(unit, None, 0, 100);
        for (start, role, _) in cases {
            let reference = Reference::new(
                module,
                fixture::range(start, start + 1),
                PathAnchor::Lexical,
                fixture::path("run"),
                role,
            );
            fixture
                .builder
                .reference(reference)
                .expect("reference accepted");
        }
        let graph = fixture.build();
        let output = published(&graph, BindingLimits::default());
        for (start, _, expected) in cases {
            let symbol = format!("rift://binding/reference/src/lib.rs@{start}-{}", start + 1);
            let reference = contribution_with_symbol(output.publication(), &symbol);
            let BindingFact::Reference { role, .. } = fact_of(reference) else {
                panic!("reference fact expected");
            };
            assert_eq!(role, expected, "role fact for {symbol}");
        }
    }

    #[test]
    fn test_publisher_types_cross_threads_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BindingPublisher>();
    }
}
