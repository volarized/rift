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
    /// One directed edge between symbols.
    Edge,
    /// A logical, timeless unit of code.
    Symbol,
    /// A physical part of a symbol, addressed by its content.
    SyntaxElement,
    /// One physical usage of a symbol.
    Occurrence,
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
    /// The analysis that produced an edge.
    Provider,
    /// The symbol an edge starts at.
    From,
    /// The symbol an edge points at.
    To,
    /// The name of a symbol, as written in source.
    Name,
    /// The selected revision of a symbol.
    Revision,
    /// The declaration, syntax, occurrence, or edge kind.
    Kind,
    /// The language identity segment.
    Language,
    /// The symbol whose declaration holds a syntax element.
    Of,
    /// The position of a syntax element's declaration among its symbol's declarations.
    Declaration,
    /// The preorder position of a syntax element in its declaration or source unit.
    Index,
    /// The syntax element enclosing a syntax element.
    Parent,
    /// The place a syntax element is written.
    Range,
    /// The source unit a range lies in, or that holds a syntax element outside every declaration.
    Unit,
    /// The first byte of a range.
    Start,
    /// One past the last byte of a range.
    End,
    /// The exact source text of a syntax element.
    Source,
    /// A symbol invokes a symbol.
    Calls,
    /// A symbol mentions a symbol.
    References,
    /// The source brings the target into scope.
    Imports,
    /// The source is another name for the target.
    Aliases,
    /// The source inherits from the target.
    Extends,
    /// The source fulfils the target's interface.
    Implements,
    /// The source declares the target.
    Declares,
    /// The source contains the target within its scope.
    Contains,
    /// The source replaces an inherited member.
    Overrides,
    /// The source reads the target's value.
    Reads,
    /// The source assigns to the target.
    Writes,
    /// The source is typed by the target.
    HasType,
    /// A syntax element that uses a symbol.
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
    /// The blank node a syntax element's `range` names.
    Range,
    /// An `rdf:Statement` blank node reifying one `coChanges` triple.
    Statement,
}

/// The value a vocabulary property points at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VocabularyValue {
    /// One resource from the listed alternatives.
    OneOf(&'static [VocabularyResource]),
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

const EDGE: VocabularyResource = VocabularyResource::Class(VocabularyClass::Edge);
const SYMBOL: VocabularyResource = VocabularyResource::Class(VocabularyClass::Symbol);
const SYNTAX_ELEMENT: VocabularyResource =
    VocabularyResource::Class(VocabularyClass::SyntaxElement);
const OCCURRENCE: VocabularyResource = VocabularyResource::Class(VocabularyClass::Occurrence);
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
            Self::Edge => "One directed edge between symbols, with its provider and derivation.",
            Self::Symbol => {
                "A logical, timeless unit of code: `rift://symbol/<owner>/<language>/<qualified path>`. Several declarations of one path are several facets of one symbol."
            }
            Self::SyntaxElement => {
                "A syntax element, written as a blank node: inside a declaration it carries `of`, `declaration` and `index`, outside every declaration `unit` and `index`. It has one `range` at the read's revision."
            }
            Self::Occurrence => {
                "One physical usage of a symbol. An occurrence names its source range and, when known, its enclosing symbol."
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
    #[expect(
        clippy::too_many_lines,
        reason = "One table defines the domain and range of every predicate."
    )]
    pub const fn definition(self) -> PropertyDefinition {
        use VocabularyResource::{Range, Statement};
        use VocabularyValue::{Integer, OneOf, Resource, String};
        let (subjects, object, description): (&'static [VocabularyResource], _, _) = match self {
            Self::Provider => (
                &[EDGE],
                String,
                "The analysis that produced the edge: `static`, `engine`, or `lsp`.",
            ),
            Self::From => (&[EDGE], Resource(SYMBOL), "The symbol the edge starts at."),
            Self::To => (&[EDGE], Resource(SYMBOL), "The symbol the edge points at."),
            Self::Name => (
                &[SYMBOL],
                String,
                "The name of the symbol, as written in source.",
            ),
            Self::Revision => (
                &[SYMBOL],
                Resource(REVISION),
                "The revision at which the server read the symbol's facts.",
            ),
            Self::Kind => (
                &[SYMBOL, SYNTAX_ELEMENT, OCCURRENCE, EDGE],
                String,
                "The kind of the symbol, syntax element, occurrence, or edge, as its provider names it.",
            ),
            Self::Language => (
                &[SYMBOL, SYNTAX_ELEMENT],
                String,
                "The language identity segment, such as `rust`.",
            ),
            Self::Of => (
                &[SYNTAX_ELEMENT, OCCURRENCE],
                Resource(SYMBOL),
                "The symbol whose declaration holds the syntax element.",
            ),
            Self::Declaration => (
                &[SYNTAX_ELEMENT],
                Integer,
                "The position of the syntax element's declaration among its symbol's declarations.",
            ),
            Self::Index => (
                &[SYNTAX_ELEMENT],
                Integer,
                "The preorder position of the syntax element in its declaration, the declaration's own syntax element at 0, or among its unit's syntax elements outside every declaration.",
            ),
            Self::Parent => (
                &[SYNTAX_ELEMENT],
                Resource(SYNTAX_ELEMENT),
                "The syntax element enclosing this syntax element.",
            ),
            Self::Range => (
                &[SYNTAX_ELEMENT, OCCURRENCE],
                Resource(Range),
                "The place the syntax element is written at the read's revision.",
            ),
            Self::Unit => (
                &[Range, SYNTAX_ELEMENT],
                Resource(SOURCE_UNIT),
                "The source unit the range lies in, or that holds a syntax element outside every declaration.",
            ),
            Self::Start => (&[Range], Integer, "The first UTF-8 byte of the range."),
            Self::End => (&[Range], Integer, "One past the last byte of the range."),
            Self::Source => (
                &[SYNTAX_ELEMENT, OCCURRENCE],
                String,
                "The exact source text at the selected range.",
            ),
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
            Self::Imports => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source brings the target into scope.",
            ),
            Self::Aliases => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source is another name for the target.",
            ),
            Self::Extends => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source inherits from the target.",
            ),
            Self::Implements => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source fulfils the target's interface.",
            ),
            Self::Declares => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source declares the target.",
            ),
            Self::Contains => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source contains the target within its scope.",
            ),
            Self::Overrides => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source replaces an inherited member.",
            ),
            Self::Reads => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source reads the target's value.",
            ),
            Self::Writes => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source assigns to the target.",
            ),
            Self::HasType => (
                &[SYMBOL],
                Resource(SYMBOL),
                "The source is typed by the target.",
            ),
            Self::Usages => (
                &[SYMBOL],
                OneOf(&[SYNTAX_ELEMENT, OCCURRENCE]),
                "A syntax element or occurrence that uses the symbol, the evidence an incoming `calls` or `references` edge was read from; its `of` names the using symbol and its `range` the place.",
            ),
            Self::IntroducedIn => (&[SYMBOL], Resource(REVISION), "The revision that added it."),
            Self::ChangedIn => (&[SYMBOL], Resource(REVISION), "A revision that changed it."),
            Self::RemovedIn => (
                &[SYMBOL],
                Resource(REVISION),
                "The revision that deleted it.",
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
