//! Projection proofs for queries that only need captured symbols.

use oxrdf::vocab::rdf;
use spargebra::Query;
use spargebra::algebra::{Expression, OrderExpression, QueryExpression};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

use super::{GraphQuery, VOCABULARY_NAMESPACE};

/// The symbol facts needed by a query that does not read other fact families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolSelection<'query> {
    /// All captured logical symbols.
    All,
    /// Captured symbols with this exact name.
    Named(&'query str),
}

impl GraphQuery {
    /// Proves the parsed expression can be evaluated using symbol facts alone.
    ///
    /// An expression that may inspect other facts requires the full acquisition path.
    #[must_use]
    pub fn symbol_selection(&self) -> Option<SymbolSelection<'_>> {
        let mut expression = match self.query() {
            Query::Select(query) => &query.expression,
            Query::Ask(query) => &query.expression,
            Query::Construct(query) => &query.expression,
            Query::Describe(_) => return None,
        };
        loop {
            expression = match expression {
                QueryExpression::Project { inner, .. }
                | QueryExpression::Distinct { inner }
                | QueryExpression::Reduced { inner }
                | QueryExpression::Slice { inner, .. } => inner,
                QueryExpression::OrderBy { inner, expression }
                    if expression.iter().all(|item| {
                        matches!(
                            item,
                            OrderExpression::Asc(Expression::Variable(_))
                                | OrderExpression::Desc(Expression::Variable(_))
                        )
                    }) =>
                {
                    inner
                }
                QueryExpression::Bgp { patterns } => return symbol_patterns(patterns),
                _ => return None,
            };
        }
    }
}

fn symbol_patterns(patterns: &[TriplePattern]) -> Option<SymbolSelection<'_>> {
    let mut name = None;
    let single_subject = patterns.first().is_some_and(|first| {
        patterns
            .iter()
            .all(|pattern| pattern.subject == first.subject)
    });
    for pattern in patterns {
        let NamedNodePattern::NamedNode(predicate) = &pattern.predicate else {
            return None;
        };
        match predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE) {
            Some("name") => {
                if let TermPattern::Literal(value) = &pattern.object {
                    name = Some(value.value());
                }
            }
            Some("revision") => {}
            Some("kind" | "language")
                if patterns
                    .iter()
                    .any(|other| other.subject == pattern.subject && is_symbol_class(other)) => {}
            _ if is_symbol_class(pattern) => {}
            _ => return None,
        }
    }
    Some(match name.filter(|_| single_subject) {
        Some(name) => SymbolSelection::Named(name),
        None => SymbolSelection::All,
    })
}

fn is_symbol_class(pattern: &TriplePattern) -> bool {
    matches!(&pattern.predicate, NamedNodePattern::NamedNode(predicate) if predicate.as_str() == rdf::TYPE.as_str())
        && matches!(&pattern.object, TermPattern::NamedNode(class) if class.as_str().strip_prefix(VOCABULARY_NAMESPACE) == Some("Symbol"))
}
