//! Required facts for queries that need the complete local semantic inventory.

use oxrdf::vocab::rdf;
use spargebra::algebra::PropertyPathExpression;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

use super::{GraphQuery, VOCABULARY_NAMESPACE};

/// Physical occurrences and their additional evidence required by a query.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OccurrenceSelection {
    /// The query can inspect exact source bytes.
    pub source: bool,
    /// The query can inspect the canonical enclosing symbol.
    pub enclosing: bool,
}

/// The semantic and physical facts needed from a complete selected domain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SemanticInventory {
    /// The query can inspect call relationships.
    pub calls: bool,
    /// The query can inspect caller-to-target reference relationships.
    pub references: bool,
    /// The query can inspect physical occurrences.
    pub occurrences: Option<OccurrenceSelection>,
}

impl GraphQuery {
    /// Finds semantic requirements of general SPARQL algebra when every code fact it
    /// can inspect is a symbol, an established relationship, or a physical reference.
    #[must_use]
    pub fn semantic_inventory(&self) -> Option<SemanticInventory> {
        self.admission().semantics.filter(|requirements| {
            requirements.calls || requirements.references || requirements.occurrences.is_some()
        })
    }
}

impl SemanticInventory {
    pub(super) fn include_patterns(&mut self, patterns: &[TriplePattern]) -> Option<()> {
        for pattern in patterns {
            let NamedNodePattern::NamedNode(predicate) = &pattern.predicate else {
                return None;
            };
            let subject = &pattern.subject;
            let name = predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE);
            match name {
                Some("name" | "revision") => {}
                Some("calls") => self.calls = true,
                Some("references") => self.references = true,
                Some("usages") => {
                    self.occurrences.get_or_insert_default();
                }
                Some("kind" | "language")
                    if patterns.iter().any(|other| {
                        other.subject == *subject && class(other) == Some("Symbol")
                    }) => {}
                Some("source" | "of" | "kind" | "range") if occurrence(patterns, subject) => {
                    let usage = self.occurrences.get_or_insert_default();
                    usage.source |= name == Some("source");
                    usage.enclosing |= name == Some("of");
                }
                Some("start" | "end" | "unit")
                    if patterns.iter().any(|other| {
                        other.object == *subject
                            && property(other) == Some("range")
                            && occurrence(patterns, &other.subject)
                    }) =>
                {
                    self.occurrences.get_or_insert_default();
                }
                None if class(pattern) == Some("Symbol") => {}
                None if class(pattern) == Some("Occurrence") => {
                    self.occurrences.get_or_insert_default();
                }
                _ => return None,
            }
        }
        Some(())
    }

    pub(super) fn include_path(&mut self, path: &PropertyPathExpression) -> Option<()> {
        let mut pending = vec![path];
        while let Some(path) = pending.pop() {
            match path {
                PropertyPathExpression::Link(predicate) => {
                    match predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE) {
                        Some("calls") => self.calls = true,
                        Some("references") => self.references = true,
                        Some("usages") => {
                            self.occurrences.get_or_insert_default();
                        }
                        _ => return None,
                    }
                }
                PropertyPathExpression::Inv(path) | PropertyPathExpression::OneOrMorePath(path) => {
                    pending.push(path);
                }
                PropertyPathExpression::Seq(left, right)
                | PropertyPathExpression::Alt(left, right) => {
                    pending.extend([left.as_ref(), right.as_ref()]);
                }
                _ => return None,
            }
        }
        Some(())
    }
}

fn occurrence(patterns: &[TriplePattern], term: &TermPattern) -> bool {
    patterns.iter().any(|pattern| {
        (pattern.subject == *term && class(pattern) == Some("Occurrence"))
            || (pattern.object == *term && property(pattern) == Some("usages"))
    })
}

fn property(pattern: &TriplePattern) -> Option<&str> {
    let NamedNodePattern::NamedNode(predicate) = &pattern.predicate else {
        return None;
    };
    predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE)
}

fn class(pattern: &TriplePattern) -> Option<&str> {
    if !matches!(&pattern.predicate, NamedNodePattern::NamedNode(predicate) if predicate.as_str() == rdf::TYPE.as_str())
    {
        return None;
    }
    let TermPattern::NamedNode(class) = &pattern.object else {
        return None;
    };
    class.as_str().strip_prefix(VOCABULARY_NAMESPACE)
}
