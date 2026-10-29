//! The bounded in-memory dataset one graph query is evaluated over.

use oxrdf::vocab::rdf;
use oxrdf::{
    BlankNode, Dataset, Graph, GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Term, Triple,
};
use rift_error::{RiftError, errors};
use rift_protocol::read::{Language, NodeId, SourceUnitId, SymbolId, TextRange};
use sha2::{Digest as _, Sha256};

use super::limits::GraphLimits;
use super::vocabulary::{VocabularyClass, VocabularyProperty};

/// IRI prefix of a symbol identity.
const SYMBOL_PREFIX: &str = "rift://symbol/";
/// IRI prefix of a node identity.
const NODE_PREFIX: &str = "rift://node/";
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

/// One place a node is written.
#[derive(Clone, Copy, Debug)]
pub struct NodeLocation<'a> {
    /// The source unit holding the node.
    pub unit: &'a SourceUnitId,
    /// The node's bytes in that unit.
    pub range: &'a TextRange,
}

/// The facts one syntax node carries.
#[derive(Clone, Copy, Debug)]
pub struct NodeFact<'a> {
    /// The node's content identity.
    pub node: &'a NodeId,
    /// The syntax kind.
    pub kind: &'a str,
    /// The node's language.
    pub language: &'a Language,
    /// The symbol the node declares, when it declares one.
    pub of: Option<&'a SymbolId>,
    /// The enclosing node, absent for a file's outermost node.
    pub parent: Option<&'a NodeId>,
    /// Where the node is written at the read's revision.
    pub location: Option<NodeLocation<'a>>,
    /// The node's exact source text.
    pub source: Option<&'a str>,
}

/// A directed edge from one symbol to another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolRelation {
    /// The source invokes the target.
    Calls,
    /// The source mentions the target.
    References,
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

/// One fact a graph dataset holds. Every IRI a fact names is typed with its class, so a
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
    /// A syntax node.
    Node(NodeFact<'a>),
    /// An edge between two symbols.
    Relation {
        /// The edge's source symbol.
        source: &'a SymbolId,
        /// What the edge states.
        relation: SymbolRelation,
        /// The edge's target symbol.
        target: &'a SymbolId,
    },
    /// A node that uses a symbol.
    Usage {
        /// The used symbol.
        symbol: &'a SymbolId,
        /// The node using it.
        node: &'a NodeId,
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
        let mut added = Graph::new();
        for triple in written.0 {
            let held = self
                .dataset
                .contains(triple.as_ref().in_graph(GraphNameRef::DefaultGraph));
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
            self.dataset
                .insert(triple.in_graph(oxrdf::GraphName::DefaultGraph));
        }
        Ok(())
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
            Self::Node(node) => node.write_triples(out)?,
            Self::Relation {
                source,
                relation,
                target,
            } => {
                let (source, target) = (out.symbol(source)?, out.symbol(target)?);
                out.link(&source, relation.property(), target);
            }
            Self::Usage { symbol, node } => {
                let (symbol, node) = (out.symbol(symbol)?, out.node(node)?);
                out.link(&symbol, Property::Usages, node);
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

impl NodeFact<'_> {
    /// Writes the node's triples, its range as a blank node labeled by its content.
    fn write_triples(&self, out: &mut TripleWriter) -> Result<(), RiftError> {
        use VocabularyProperty as Property;
        let node = out.node(self.node)?;
        out.literal(&node, Property::Kind, self.kind);
        out.literal(&node, Property::Language, &self.language.identity_segment());
        if let Some(symbol) = self.of {
            let symbol = out.symbol(symbol)?;
            out.link(&node, Property::Of, symbol);
        }
        if let Some(parent) = self.parent {
            let parent = out.node(parent)?;
            out.link(&node, Property::Parent, parent);
        }
        if let Some(source) = self.source {
            out.literal(&node, Property::Source, source);
        }
        if let Some(NodeLocation { unit, range }) = self.location {
            let unit = out.typed(
                identity_iri(&unit.0, SOURCE_UNIT_PREFIX)?,
                VocabularyClass::SourceUnit,
            );
            let (start, end) = (range.start.to_string(), range.end.to_string());
            let location = content_blank_node(&[node.as_str(), unit.as_str(), &start, &end]);
            out.link(&node, Property::Range, location.clone());
            out.link(&location, Property::Unit, unit);
            out.link(&location, Property::Start, Literal::from(range.start));
            out.link(&location, Property::End, Literal::from(range.end));
        }
        Ok(())
    }
}

impl SymbolRelation {
    const fn property(self) -> VocabularyProperty {
        match self {
            Self::Calls => VocabularyProperty::Calls,
            Self::References => VocabularyProperty::References,
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

/// The triples one fact writes. Every IRI it names is typed with its class.
#[derive(Default)]
struct TripleWriter(Vec<Triple>);

impl TripleWriter {
    /// Writes `iri rdf:type class` and returns `iri`.
    fn typed(&mut self, iri: NamedNode, class: VocabularyClass) -> NamedNode {
        self.0
            .push(Triple::new(iri.clone(), rdf::TYPE, class.iri()));
        iri
    }

    fn symbol(&mut self, symbol: &SymbolId) -> Result<NamedNode, RiftError> {
        let iri = identity_iri(&symbol.0, SYMBOL_PREFIX)?;
        Ok(self.typed(iri, VocabularyClass::Symbol))
    }

    fn node(&mut self, node: &NodeId) -> Result<NamedNode, RiftError> {
        let iri = identity_iri(&node.0, NODE_PREFIX)?;
        Ok(self.typed(iri, VocabularyClass::Node))
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
    fn literal(&mut self, subject: &NamedNode, property: VocabularyProperty, value: &str) {
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
