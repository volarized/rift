//! The bounded in-memory dataset one graph query is evaluated over.

use oxrdf::vocab::rdf;
use oxrdf::{
    BlankNode, Dataset, Graph, GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Term, Triple,
};
use rift_error::{RiftError, errors};
use rift_protocol::read::{Language, SourceUnitId, SymbolId, TextRange};
use sha2::{Digest as _, Sha256};

use super::limits::GraphLimits;
use super::vocabulary::{VocabularyClass, VocabularyProperty};

mod flows;
mod retrieval;

pub use flows::{FlowCallSite, QueryFlows};

/// IRI prefix of a symbol identity.
const SYMBOL_PREFIX: &str = "rift://symbol/";
/// IRI prefix of a source unit identity.
const SOURCE_UNIT_PREFIX: &str = "rift://source/";
/// IRI prefix of a revision identity.
const REVISION_PREFIX: &str = "rift://revision/";
/// IRI prefix of a git commit revision, which the dataset also types `Commit`.
const COMMIT_PREFIX: &str = "rift://revision/git/";

/// One revision a history or dependency fact names: `rift://revision/git/<commit sha>`, a
/// package or runtime release, or a served working tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionIri(NamedNode);

impl RevisionIri {
    /// Accepts `value` as a revision IRI.
    ///
    /// # Errors
    ///
    /// Returns `graph_identity_invalid` when `value` is no IRI under `rift://revision/`.
    pub fn parse(value: &str) -> Result<Self, RiftError> {
        identity_iri(value, REVISION_PREFIX).map(Self)
    }

    /// The IRI as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Whether this revision is a git commit.
    #[must_use]
    pub fn is_commit(&self) -> bool {
        self.as_str().starts_with(COMMIT_PREFIX)
    }
}

/// Where one syntax element sits in the model. A syntax element has no identity of its own: its address
/// names it, and the dataset writes it as a blank node labeled by that address.
#[derive(Clone, Copy, Debug)]
pub enum SyntaxAddress<'a> {
    /// A syntax element inside one declaration of a symbol.
    Declaration {
        /// The symbol the declaration belongs to.
        symbol: &'a SymbolId,
        /// The declaration's position among the symbol's declarations.
        declaration: u64,
        /// The syntax element's preorder position inside the declaration, its own syntax element at 0.
        index: u64,
    },
    /// A syntax element outside every declaration, such as a file's outermost syntax element.
    Unit {
        /// The source unit holding it.
        unit: &'a SourceUnitId,
        /// The syntax element's preorder position among the unit's syntax elements outside every declaration.
        index: u64,
    },
}

/// The place a syntax element is written at the read's revision.
#[derive(Clone, Copy, Debug)]
pub struct SyntaxLocation<'a> {
    /// The source unit holding the syntax element.
    pub unit: &'a SourceUnitId,
    /// The syntax element's bytes in that unit.
    pub range: &'a TextRange,
}

/// The facts one syntax element carries.
#[derive(Clone, Copy, Debug)]
pub struct SyntaxFact<'a> {
    /// Where the syntax element sits.
    pub address: SyntaxAddress<'a>,
    /// The syntax kind.
    pub kind: &'a str,
    /// The syntax element's language.
    pub language: &'a Language,
    /// The enclosing syntax element, absent for a file's outermost syntax element.
    pub parent: Option<SyntaxAddress<'a>>,
    /// Where the syntax element is written at the read's revision.
    pub location: Option<SyntaxLocation<'a>>,
    /// The syntax element's exact source text.
    pub source: Option<&'a str>,
}

/// A directed edge from one symbol to another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolRelation {
    /// The source invokes the target.
    Calls,
    /// The source mentions the target.
    References,
    /// The source brings the target into scope.
    Imports,
    /// The source is another name for the target.
    Aliases,
    /// The source inherits from the target.
    Extends,
    /// The source fulfils the target's interface.
    Implements,
    /// The source contains the target within its scope.
    Contains,
    /// The source replaces the target inherited from a supertype.
    Overrides,
    /// The source declares the target.
    Declares,
    /// The source reads the target's value.
    Reads,
    /// The source assigns to the target.
    Writes,
    /// The source is typed by the target.
    HasType,
    /// The source was renamed from the target.
    RenamedFrom,
}

/// What a revision did to a symbol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryEvent {
    /// The revision added the symbol.
    Introduced,
    /// The revision changed the symbol.
    Changed,
    /// The revision deleted the symbol.
    Removed,
}

/// The address of a logical symbol in one captured publication.
#[derive(Clone, Copy, Debug)]
pub enum SymbolAddress<'a> {
    /// An established canonical symbol address.
    Established(&'a SymbolId),
    /// The original contribution of a symbol without an established identity.
    Unestablished(&'a rift_core::ContributionReference),
}

/// One fact a graph dataset holds. Every resource a fact names is typed with its class, so a
/// query reaches it through `rdf:type` whichever fact introduced it.
#[derive(Clone, Copy, Debug)]
pub enum GraphFact<'a> {
    /// A symbol with its kind and language.
    Symbol {
        /// The symbol identity.
        symbol: &'a SymbolId,
        /// The declaration kind.
        kind: &'a str,
        /// The symbol's language.
        language: &'a Language,
    },
    /// The portable presentation of a captured logical symbol.
    SymbolDetails {
        /// The symbol's address in this selection.
        address: SymbolAddress<'a>,
        /// The symbol's name, as written in source.
        name: &'a str,
        /// The declaration kind.
        kind: &'a str,
        /// The symbol's language.
        language: &'a Language,
        /// The original selected revision.
        revision: &'a RevisionIri,
    },
    /// A syntax element.
    SyntaxElement(SyntaxFact<'a>),
    /// An edge between two symbols.
    Relation {
        /// The edge's source symbol.
        source: &'a SymbolId,
        /// What the edge states.
        relation: SymbolRelation,
        /// The edge's target symbol.
        target: &'a SymbolId,
    },
    /// A syntax element that uses a symbol.
    Usage {
        /// The used symbol.
        symbol: &'a SymbolId,
        /// The syntax element using it.
        syntax_element: SyntaxAddress<'a>,
    },
    /// One physical usage, preserving the identity the provider gave this occurrence.
    Occurrence {
        /// The original record key within the selected provider and revision.
        key: &'a str,
        /// Target, enclosing symbol and source span.
        occurrence: &'a rift_protocol::edge_read::QueryOccurrence,
        /// Exact source bytes at this occurrence, when acquired.
        source: Option<&'a str>,
    },
    /// A revision's effect on a symbol.
    History {
        /// The symbol.
        symbol: &'a SymbolId,
        /// What the revision did.
        event: HistoryEvent,
        /// The revision.
        revision: &'a RevisionIri,
    },
    /// Two symbols changed in the same commits.
    CoChange {
        /// One symbol.
        symbol: &'a SymbolId,
        /// The symbol it changed with.
        other: &'a SymbolId,
        /// How many commits changed both.
        commit_count: u64,
    },
    /// A workspace revision depends on a package or runtime revision.
    Uses {
        /// The depending revision.
        revision: &'a RevisionIri,
        /// The revision depended on.
        dependency: &'a RevisionIri,
    },
}

/// The bounded set of triples one graph query is evaluated over.
#[derive(Debug)]
pub struct GraphDataset {
    dataset: Dataset,
    limits: GraphLimits,
}

impl GraphDataset {
    /// Starts an empty dataset bounded by `limits`.
    pub fn builder(limits: GraphLimits) -> GraphDatasetBuilder {
        GraphDatasetBuilder {
            dataset: Dataset::new(),
            limits,
            literal_bytes: 0,
        }
    }

    /// The number of triples held.
    #[must_use]
    pub fn triple_count(&self) -> usize {
        self.dataset.len()
    }

    /// The bounds this dataset was built and is evaluated within.
    #[must_use]
    pub const fn limits(&self) -> &GraphLimits {
        &self.limits
    }

    /// The triples, for evaluation.
    pub(super) const fn triples(&self) -> &Dataset {
        &self.dataset
    }

    /// A dataset holding arbitrary triples, for conformance cases outside the vocabulary.
    #[cfg(test)]
    pub(super) fn from_triples(
        triples: impl IntoIterator<Item = Triple>,
        limits: GraphLimits,
    ) -> Self {
        let dataset = triples
            .into_iter()
            .map(|triple| triple.in_graph(oxrdf::GraphName::DefaultGraph))
            .collect();
        Self { dataset, limits }
    }
}

/// Accumulates [`GraphFact`]s within the triple and literal byte bounds.
#[derive(Debug)]
#[must_use]
pub struct GraphDatasetBuilder {
    dataset: Dataset,
    limits: GraphLimits,
    literal_bytes: usize,
}

impl GraphDatasetBuilder {
    /// Adds one fact's triples, all or none.
    ///
    /// A triple the dataset already holds costs nothing, so repeating a fact is free.
    ///
    /// # Errors
    ///
    /// - `graph_identity_invalid` when an identity is not an IRI of its expected form.
    /// - `graph_dataset_limit` when the new triples would pass the triple or literal byte bound;
    ///   the dataset then holds none of them.
    pub fn insert(&mut self, fact: GraphFact<'_>) -> Result<(), RiftError> {
        let mut written = TripleWriter::default();
        fact.write_triples(&mut written)?;
        self.insert_triples(written.0, GraphNameRef::DefaultGraph)
    }

    fn insert_triples(
        &mut self,
        triples: Vec<Triple>,
        graph: GraphNameRef<'_>,
    ) -> Result<(), RiftError> {
        let mut added = Graph::new();
        for triple in triples {
            let held = self.dataset.contains(triple.as_ref().in_graph(graph));
            if !held {
                added.insert(triple);
            }
        }
        let literal_bytes = added
            .iter()
            .map(|triple| literal_bytes(&triple))
            .sum::<usize>();
        self.admit(added.len(), literal_bytes)?;
        self.literal_bytes += literal_bytes;
        for triple in &added {
            self.dataset.insert(triple.in_graph(graph));
        }
        Ok(())
    }

    /// Adds one syntax element from the existing revision-pinned syntax read.
    ///
    /// # Errors
    /// Returns a graph admission refusal for an invalid or oversized syntax element.
    pub fn insert_declared_syntax_element(
        &mut self,
        symbol: &SymbolId,
        syntax_element: &rift_protocol::read::SyntaxElement,
        include_source: bool,
    ) -> Result<(), RiftError> {
        let Some(declaration) = syntax_element.declaration else {
            return Ok(());
        };
        let address = |index| SyntaxAddress::Declaration {
            symbol,
            declaration,
            index,
        };
        self.insert(GraphFact::SyntaxElement(SyntaxFact {
            address: address(syntax_element.index),
            kind: &syntax_element.kind.0,
            language: &syntax_element.language,
            parent: syntax_element.parent.map(address),
            location: Some(SyntaxLocation {
                unit: &syntax_element.unit,
                range: &syntax_element.range,
            }),
            source: include_source.then_some(syntax_element.source.as_str()),
        }))
    }

    /// Adds the item extent and optional excerpt of a captured physical declaration.
    ///
    /// The record position distinguishes declarations within this captured publication.
    ///
    /// # Errors
    /// Returns an identity or graph admission refusal.
    pub fn insert_declaration_excerpt(
        &mut self,
        id: &SymbolId,
        position: usize,
        location: SyntaxLocation<'_>,
        source: Option<&str>,
    ) -> Result<(), RiftError> {
        use VocabularyProperty as Property;

        let mut out = TripleWriter::default();
        let symbol = out.symbol(id)?;
        let position = position.to_string();
        let syntax_element = out.typed(
            content_blank_node(&[symbol.as_str(), "captured-declaration", &position]),
            VocabularyClass::SyntaxElement,
        );
        out.link(&syntax_element, Property::Of, symbol);
        out.link(&syntax_element, Property::Index, Literal::from(0_u64));
        out.location(&syntax_element, location.unit, location.range)?;
        if let Some(source) = source {
            out.literal(&syntax_element, Property::Source, source);
        }
        self.insert_triples(out.0, GraphNameRef::DefaultGraph)
    }

    /// Publishes the default local facts under their named graph.
    ///
    /// # Errors
    /// Refuses a named graph that exceeds the shared dataset bounds.
    pub fn publish_local_graph(&mut self) -> Result<(), RiftError> {
        let graph = NamedNode::new_unchecked(super::LOCAL_GRAPH);
        let triples = self
            .dataset
            .quads_for_graph_name(GraphNameRef::DefaultGraph)
            .map(|quad| Triple::new(quad.subject, quad.predicate, quad.object))
            .collect();
        self.insert_triples(triples, (&graph).into())
    }

    /// Accepts the dataset.
    #[must_use]
    pub fn build(self) -> GraphDataset {
        GraphDataset {
            dataset: self.dataset,
            limits: self.limits,
        }
    }

    /// Refuses `triples` new triples carrying `literal_bytes` past either bound.
    fn admit(&self, triples: usize, literal_bytes: usize) -> Result<(), RiftError> {
        let required_triples = self.dataset.len().saturating_add(triples);
        let required_bytes = self.literal_bytes.saturating_add(literal_bytes);
        let crossed = [
            ("triples", required_triples, self.limits.triples_max()),
            (
                "literal bytes",
                required_bytes,
                self.limits.literal_bytes_max(),
            ),
        ];
        match crossed
            .into_iter()
            .find(|(_, required, limit)| required > limit)
        {
            Some((bound, required, limit)) => errors::search::graph_dataset_limit()
                .bound(bound)
                .limit(limit)
                .required(required)
                .fail(),
            None => Ok(()),
        }
    }
}

impl GraphFact<'_> {
    /// Writes this fact's triples.
    fn write_triples(&self, out: &mut TripleWriter) -> Result<(), RiftError> {
        use VocabularyProperty as Property;
        match *self {
            Self::Symbol {
                symbol,
                kind,
                language,
            } => {
                let symbol = out.symbol(symbol)?;
                out.literal(&symbol, Property::Kind, kind);
                out.literal(&symbol, Property::Language, &language.identity_segment());
            }
            Self::SymbolDetails {
                address,
                name,
                kind,
                language,
                revision,
            } => {
                let subject: NamedOrBlankNode = match address {
                    SymbolAddress::Established(symbol) => out.symbol(symbol)?.into(),
                    SymbolAddress::Unestablished(reference) => out
                        .typed(
                            content_blank_node(&[
                                "symbol",
                                reference.provider().as_str(),
                                reference.symbol().as_str(),
                            ]),
                            VocabularyClass::Symbol,
                        )
                        .into(),
                };
                out.literal(&subject, Property::Name, name);
                out.literal(&subject, Property::Kind, kind);
                out.literal(&subject, Property::Language, &language.identity_segment());
                let revision = out.revision(revision);
                out.link(&subject, Property::Revision, revision);
            }
            Self::SyntaxElement(syntax_element) => syntax_element.write_triples(out)?,
            Self::Relation {
                source,
                relation,
                target,
            } => {
                let (source, target) = (out.symbol(source)?, out.symbol(target)?);
                out.link(&source, relation.property(), target);
            }
            Self::Usage {
                symbol,
                syntax_element,
            } => {
                let (symbol, syntax_element) =
                    (out.symbol(symbol)?, out.syntax_element(syntax_element)?);
                out.link(&symbol, Property::Usages, syntax_element);
            }
            Self::Occurrence {
                key,
                occurrence,
                source,
            } => {
                let target = out.symbol(&occurrence.target)?;
                let usage = out.typed(
                    content_blank_node(&["occurrence", key]),
                    VocabularyClass::Occurrence,
                );
                out.link(&target, Property::Usages, usage.clone());
                out.literal(&usage, Property::Kind, occurrence.kind.as_ref());
                out.location(&usage, &occurrence.source.unit, &occurrence.source.range)?;
                if let Some(source) = source {
                    out.literal(&usage, Property::Source, source);
                }
                if let Some(enclosing) = &occurrence.enclosing {
                    let enclosing = out.symbol(enclosing)?;
                    out.link(&usage, Property::Of, enclosing);
                }
            }
            Self::History {
                symbol,
                event,
                revision,
            } => {
                let (symbol, revision) = (out.symbol(symbol)?, out.revision(revision));
                out.link(&symbol, event.property(), revision);
            }
            Self::CoChange {
                symbol,
                other,
                commit_count,
            } => {
                let (symbol, other) = (out.symbol(symbol)?, out.symbol(other)?);
                out.co_change(symbol, other, commit_count);
            }
            Self::Uses {
                revision,
                dependency,
            } => {
                let (revision, dependency) = (out.revision(revision), out.revision(dependency));
                out.link(&revision, Property::Uses, dependency);
            }
        }
        Ok(())
    }
}

impl SyntaxFact<'_> {
    /// Writes the syntax element's triples, its range as a blank node labeled by its syntax element and bytes.
    fn write_triples(&self, out: &mut TripleWriter) -> Result<(), RiftError> {
        use VocabularyProperty as Property;
        let syntax_element = out.syntax_element(self.address)?;
        out.literal(&syntax_element, Property::Kind, self.kind);
        out.literal(
            &syntax_element,
            Property::Language,
            &self.language.identity_segment(),
        );
        if let Some(parent) = self.parent {
            let parent = out.syntax_element(parent)?;
            out.link(&syntax_element, Property::Parent, parent);
        }
        if let Some(source) = self.source {
            out.literal(&syntax_element, Property::Source, source);
        }
        if let Some(SyntaxLocation { unit, range }) = self.location {
            out.location(&syntax_element, unit, range)?;
        }
        Ok(())
    }
}

impl SymbolRelation {
    const fn property(self) -> VocabularyProperty {
        match self {
            Self::Calls => VocabularyProperty::Calls,
            Self::References => VocabularyProperty::References,
            Self::Imports => VocabularyProperty::Imports,
            Self::Aliases => VocabularyProperty::Aliases,
            Self::Extends => VocabularyProperty::Extends,
            Self::Implements => VocabularyProperty::Implements,
            Self::Contains => VocabularyProperty::Contains,
            Self::Overrides => VocabularyProperty::Overrides,
            Self::Declares => VocabularyProperty::Declares,
            Self::Reads => VocabularyProperty::Reads,
            Self::Writes => VocabularyProperty::Writes,
            Self::HasType => VocabularyProperty::HasType,
            Self::RenamedFrom => VocabularyProperty::RenamedFrom,
        }
    }
}

impl HistoryEvent {
    const fn property(self) -> VocabularyProperty {
        match self {
            Self::Introduced => VocabularyProperty::IntroducedIn,
            Self::Changed => VocabularyProperty::ChangedIn,
            Self::Removed => VocabularyProperty::RemovedIn,
        }
    }
}

/// The triples one fact writes. Every resource it names is typed with its class.
#[derive(Default)]
struct TripleWriter(Vec<Triple>);

impl TripleWriter {
    /// Writes `resource rdf:type class` and returns `resource`.
    fn typed<R: Clone + Into<NamedOrBlankNode>>(
        &mut self,
        resource: R,
        class: VocabularyClass,
    ) -> R {
        self.0
            .push(Triple::new(resource.clone(), rdf::TYPE, class.iri()));
        resource
    }

    fn unit(&mut self, unit: &SourceUnitId) -> Result<NamedNode, RiftError> {
        let iri = identity_iri(&unit.0, SOURCE_UNIT_PREFIX)?;
        Ok(self.typed(iri, VocabularyClass::SourceUnit))
    }

    fn symbol(&mut self, symbol: &SymbolId) -> Result<NamedNode, RiftError> {
        let iri = identity_iri(&symbol.0, SYMBOL_PREFIX)?;
        Ok(self.typed(iri, VocabularyClass::Symbol))
    }

    /// Writes the syntax element at `address` as a blank node labeled by the address, with the triples
    /// that place it: `of`, `declaration` and `index` inside a declaration, `unit` and `index`
    /// outside every declaration.
    fn syntax_element(&mut self, address: SyntaxAddress<'_>) -> Result<BlankNode, RiftError> {
        use VocabularyProperty as Property;
        let (owner, declaration, index) = match address {
            SyntaxAddress::Declaration {
                symbol,
                declaration,
                index,
            } => (self.symbol(symbol)?, Some(declaration), index),
            SyntaxAddress::Unit { unit, index } => (self.unit(unit)?, None, index),
        };
        let (declared, indexed) = (
            declaration.map(|value| value.to_string()),
            index.to_string(),
        );
        let label = [owner.as_str(), declared.as_deref().unwrap_or("-"), &indexed];
        let syntax_element = self.typed(content_blank_node(&label), VocabularyClass::SyntaxElement);
        match declaration {
            Some(declaration) => {
                self.link(&syntax_element, Property::Of, owner);
                self.link(
                    &syntax_element,
                    Property::Declaration,
                    Literal::from(declaration),
                );
            }
            None => self.link(&syntax_element, Property::Unit, owner),
        }
        self.link(&syntax_element, Property::Index, Literal::from(index));
        Ok(syntax_element)
    }

    fn location(
        &mut self,
        subject: &BlankNode,
        unit: &SourceUnitId,
        range: &TextRange,
    ) -> Result<(), RiftError> {
        use VocabularyProperty as Property;
        let unit = self.unit(unit)?;
        let (start, end) = (range.start.to_string(), range.end.to_string());
        let location = content_blank_node(&[subject.as_str(), unit.as_str(), &start, &end]);
        self.link(subject, Property::Range, location.clone());
        self.link(&location, Property::Unit, unit);
        self.link(&location, Property::Start, Literal::from(range.start));
        self.link(&location, Property::End, Literal::from(range.end));
        Ok(())
    }

    /// Types a revision `Revision`, and `Commit` when it is a git commit.
    fn revision(&mut self, revision: &RevisionIri) -> NamedNode {
        let iri = self.typed(revision.0.clone(), VocabularyClass::Revision);
        if revision.is_commit() {
            self.typed(iri, VocabularyClass::Commit)
        } else {
            iri
        }
    }

    /// Writes `subject property object`.
    fn link(
        &mut self,
        subject: &(impl Clone + Into<NamedOrBlankNode>),
        property: VocabularyProperty,
        object: impl Into<Term>,
    ) {
        self.0
            .push(Triple::new(subject.clone(), property.iri(), object));
    }

    /// Writes `subject property "value"`.
    fn literal(
        &mut self,
        subject: &(impl Clone + Into<NamedOrBlankNode>),
        property: VocabularyProperty,
        value: &str,
    ) {
        self.link(
            subject,
            property,
            Literal::new_simple_literal(value.to_owned()),
        );
    }

    /// Writes `a :coChanges b` and the `rdf:Statement` reifying it with its commit count.
    fn co_change(&mut self, symbol: NamedNode, other: NamedNode, commit_count: u64) {
        let predicate = VocabularyProperty::CoChanges.iri();
        let statement = content_blank_node(&[predicate.as_str(), symbol.as_str(), other.as_str()]);
        self.link(&symbol, VocabularyProperty::CoChanges, other.clone());
        let reified: [(NamedNode, Term); 5] = [
            (rdf::TYPE, rdf::STATEMENT.into()),
            (rdf::SUBJECT, symbol.into()),
            (rdf::PREDICATE, predicate.into()),
            (rdf::OBJECT, other.into()),
            (
                VocabularyProperty::CommitCount.iri(),
                Literal::from(commit_count).into(),
            ),
        ];
        let reified = reified
            .into_iter()
            .map(|(predicate, object)| Triple::new(statement.clone(), predicate, object));
        self.0.extend(reified);
    }
}

/// A blank node labeled by the SHA-256 of `parts`, so one fact always mints one label.
fn content_blank_node(parts: &[&str]) -> BlankNode {
    let digest = parts.iter().fold(Sha256::new(), |hasher, part| {
        hasher
            .chain_update((part.len() as u64).to_le_bytes())
            .chain_update(part.as_bytes())
    });
    BlankNode::new_unchecked(format!(
        "b{}",
        data_encoding::HEXLOWER.encode(&digest.finalize())
    ))
}

/// The literal text bytes one triple carries.
fn literal_bytes(triple: &Triple) -> usize {
    match &triple.object {
        Term::Literal(literal) => literal.value().len(),
        Term::NamedNode(_) | Term::BlankNode(_) => 0,
    }
}

/// Accepts `value` as an IRI under `prefix`.
fn identity_iri(value: &str, prefix: &str) -> Result<NamedNode, RiftError> {
    match NamedNode::new(value.to_owned()) {
        Ok(iri) if value.len() > prefix.len() && value.starts_with(prefix) => Ok(iri),
        Ok(_) | Err(_) => errors::search::graph_identity_invalid()
            .identity(value)
            .expected(format!("{prefix}..."))
            .fail(),
    }
}
