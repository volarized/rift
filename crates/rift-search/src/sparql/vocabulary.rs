//! The fixed vocabulary every graph dataset is written in.
//!
//! The two enums below are the vocabulary's single source: each variant's IRI is
//! [`VOCABULARY_NAMESPACE`] followed by the variant's name, and each `definition` arm is one
//! row of the table a query author reads. A query names a term as `:Symbol` or `:calls`,
//! since the parser predeclares the empty prefix as the namespace.

use oxrdf::NamedNode;
use strum::{IntoStaticStr, VariantArray};

/// The namespace every vocabulary term is minted under.
pub const VOCABULARY_NAMESPACE: &str = "urn:rift:vocabulary:";

/// One class a graph resource is typed with through `rdf:type`.
#[derive(Clone, Copy, Debug, Eq, Hash, IntoStaticStr, PartialEq, VariantArray)]
pub enum VocabularyClass {
    /// A logical, timeless unit of code.
    Symbol,
    /// A physical part of a symbol, addressed by its content.
    Node,
    /// A point on the history axis.
    Revision,
    /// A physical file whose bytes are read at a revision.
    SourceUnit,
    /// A git commit; every commit is also a revision.
    Commit,
}

/// One property a graph triple carries as its predicate.
#[derive(Clone, Copy, Debug, Eq, Hash, IntoStaticStr, PartialEq, VariantArray)]
#[strum(serialize_all = "camelCase")]
pub enum VocabularyProperty {
    /// The declaration or syntax kind.
    Kind,
    /// The language identity segment.
    Language,
    /// The symbol a node declares.
    Of,
    /// The node enclosing a node.
    Parent,
    /// One place a node is written.
    Range,
    /// The source unit a range lies in.
    Unit,
    /// The first byte of a range.
    Start,
    /// One past the last byte of a range.
    End,
    /// The exact source text of a node.
    Source,
    /// A symbol invokes a symbol.
    Calls,
    /// A symbol mentions a symbol.
    References,
    /// A node that uses a symbol.
    Usages,
    /// The revision that added a symbol.
    IntroducedIn,
    /// A revision that changed a symbol.
    ChangedIn,
    /// The revision that deleted a symbol.
    RemovedIn,
    /// The symbol a symbol was renamed from.
    RenamedFrom,
    /// A symbol changed in the same commits as a symbol.
    CoChanges,
    /// The commits a reified `coChanges` statement counts.
    CommitCount,
    /// A revision depends on a revision.
    Uses,
}

/// A resource a vocabulary property connects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VocabularyResource {
    /// An IRI typed with this class.
    Class(VocabularyClass),
    /// The blank node a node's `range` names.
    Range,
    /// An `rdf:Statement` blank node reifying one `coChanges` triple.
    Statement,
}

/// The value a vocabulary property points at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VocabularyValue {
    /// A resource.
    Resource(VocabularyResource),
    /// An `xsd:string` literal.
    String,
    /// An `xsd:integer` literal.
    Integer,
}

/// One row of the property table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropertyDefinition {
    /// The resources the property may describe.
    pub subjects: &'static [VocabularyResource],
    /// What the property points at.
    pub object: VocabularyValue,
    /// What one triple with this predicate states.
    pub description: &'static str,
}

const SYMBOL: VocabularyResource = VocabularyResource::Class(VocabularyClass::Symbol);
const NODE: VocabularyResource = VocabularyResource::Class(VocabularyClass::Node);
const REVISION: VocabularyResource = VocabularyResource::Class(VocabularyClass::Revision);
const SOURCE_UNIT: VocabularyResource = VocabularyResource::Class(VocabularyClass::SourceUnit);

impl VocabularyClass {
    /// The class IRI, such as `urn:rift:vocabulary:Symbol`.
    #[must_use]
    pub fn iri(self) -> NamedNode {
        let name: &'static str = self.into();
        NamedNode::new_unchecked(format!("{VOCABULARY_NAMESPACE}{name}"))
    }

    /// What a resource of this class is, and the IRI form it carries.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Symbol => {
                "A logical, timeless unit of code: `rift://symbol/<owner>/<language>/<qualified path>`. Several declarations of one path are several nodes of one symbol."
            }
            Self::Node => {
                "A physical part of a symbol: `rift://node/<language>/<sha256>` over its language, kind and exact bytes. Equal bytes share one node; each place they are written is one `range`."
            }
            Self::Revision => {
                "A point on the history axis: `rift://revision/git/<commit sha>`, a package or runtime release such as `rift://revision/cargo/crates.io/demo@1.0.0`, or a served working tree `rift://revision/tree/<digest>`."
            }
            Self::SourceUnit => {
                "A physical file with a revision-free identity, such as `rift://source/project/src/lib.rs`."
            }
            Self::Commit => {
                "A git revision `rift://revision/git/<commit sha>`, typed both `Commit` and `Revision`."
            }
        }
    }
}

impl VocabularyProperty {
    /// The property IRI, such as `urn:rift:vocabulary:calls`.
    #[must_use]
    pub fn iri(self) -> NamedNode {
        let name: &'static str = self.into();
        NamedNode::new_unchecked(format!("{VOCABULARY_NAMESPACE}{name}"))
    }

    /// The table row: what the property connects and what one triple states.
    #[must_use]
    pub const fn definition(self) -> PropertyDefinition {
        use VocabularyResource::{Range, Statement};
        use VocabularyValue::{Integer, Resource, String};
        let (subjects, object, description): (&'static [VocabularyResource], _, _) = match self {
            Self::Kind => (
                &[SYMBOL, NODE],
                String,
                "The declaration kind of a symbol, or the syntax kind of a node, as its language's provider names it.",
            ),
            Self::Language => (
                &[SYMBOL, NODE],
                String,
                "The language identity segment, such as `rust`.",
            ),
            Self::Of => (&[NODE], Resource(SYMBOL), "The symbol the node declares."),
            Self::Parent => (
                &[NODE],
                Resource(NODE),
                "The node enclosing this node in its syntax tree.",
            ),
            Self::Range => (
                &[NODE],
                Resource(Range),
                "One place the node is written at the read's revision.",
            ),
            Self::Unit => (
                &[Range],
                Resource(SOURCE_UNIT),
                "The source unit the range lies in.",
            ),
            Self::Start => (&[Range], Integer, "The first UTF-8 byte of the range."),
            Self::End => (&[Range], Integer, "One past the last byte of the range."),
            Self::Source => (&[NODE], String, "The exact source text of the node."),
            Self::Calls => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The symbol invokes the target.",
            ),
            Self::References => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The symbol mentions the target.",
            ),
            Self::Usages => (
                &[SYMBOL],
                Resource(NODE),
                "A node that uses the symbol: the evidence an incoming `calls` or `references` edge was read from.",
            ),
            Self::IntroducedIn => (
                &[SYMBOL],
                Resource(REVISION),
                "The revision that added the symbol.",
            ),
            Self::ChangedIn => (
                &[SYMBOL],
                Resource(REVISION),
                "A revision that changed the symbol.",
            ),
            Self::RemovedIn => (
                &[SYMBOL],
                Resource(REVISION),
                "The revision that deleted the symbol.",
            ),
            Self::RenamedFrom => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The symbol this symbol was renamed from.",
            ),
            Self::CoChanges => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The two symbols changed in the same commits. An `rdf:Statement` reifying the triple carries `commitCount`.",
            ),
            Self::CommitCount => (
                &[Statement],
                Integer,
                "The number of commits that changed both symbols of the reified `coChanges` triple.",
            ),
            Self::Uses => (
                &[REVISION],
                Resource(REVISION),
                "The workspace revision depends on the package or runtime revision.",
            ),
        };
        PropertyDefinition {
            subjects,
            object,
            description,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{VOCABULARY_NAMESPACE, VocabularyClass, VocabularyProperty};
    use strum::VariantArray as _;

    #[test]
    fn test_vocabulary_terms_are_distinct_documented_iris() {
        let classes = VocabularyClass::VARIANTS
            .iter()
            .map(|class| (class.iri(), class.description()));
        let properties = VocabularyProperty::VARIANTS
            .iter()
            .map(|property| (property.iri(), property.definition().description));
        let mut iris = Vec::new();
        for (iri, description) in classes.chain(properties) {
            let name = iri
                .as_str()
                .strip_prefix(VOCABULARY_NAMESPACE)
                .unwrap_or_default();
            let valid = oxrdf::NamedNode::new(iri.as_str().to_owned()).is_ok();
            assert!(
                valid && !name.is_empty() && !description.is_empty(),
                "term {iri} must be a documented IRI under the namespace"
            );
            iris.push(iri.as_str().to_owned());
        }
        let count = iris.len();
        iris.sort_unstable();
        iris.dedup();
        assert_eq!(iris.len(), count, "vocabulary terms must not share an IRI");
        assert_eq!(
            VocabularyClass::SourceUnit.iri().as_str(),
            "urn:rift:vocabulary:SourceUnit"
        );
        assert_eq!(
            VocabularyProperty::IntroducedIn.iri().as_str(),
            "urn:rift:vocabulary:introducedIn"
        );
    }
}
