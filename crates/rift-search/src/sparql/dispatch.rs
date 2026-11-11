//! Translation of an anchored query to the accepted project's local identity.

use oxrdf::NamedNode;
use rift_error::{RiftError, errors};
use rift_protocol::read::SymbolId;
use spargebra::Query;
use spargebra::algebra::{AggregateExpression, Expression, OrderExpression, QueryExpression};
use spargebra::term::TermPattern;

use super::GraphQuery;

impl GraphQuery {
    /// Serializes this query with its proven adjacency seed expressed in the backend's identity.
    ///
    /// # Errors
    ///
    /// Returns a query feature error when the expression has no provable adjacency seed.
    pub fn dispatched_adjacency(&self, local: &SymbolId) -> Result<String, RiftError> {
        let selected = self.adjacency_selection().ok_or_else(dispatch_failure)?;
        let replacement = NamedNode::new(local.as_str().to_owned()).map_err(|error| {
            errors::search::query_syntax_invalid()
                .detail(error.to_string())
                .error()
        })?;
        let mut query = self.query().clone();
        let root = match &mut query {
            Query::Select(query) => &mut query.expression,
            Query::Ask(query) => &mut query.expression,
            Query::Construct(query) => &mut query.expression,
            Query::Describe(_) => return Err(dispatch_failure()),
        };
        let mut pending = vec![root];
        while let Some(expression) = pending.pop() {
            match expression {
                QueryExpression::Project { inner, .. }
                | QueryExpression::Distinct { inner }
                | QueryExpression::Reduced { inner }
                | QueryExpression::Slice { inner, .. } => pending.push(inner),
                QueryExpression::Group {
                    inner, aggregates, ..
                } if aggregates.iter().all(|(_, aggregate)| {
                    matches!(
                        aggregate,
                        AggregateExpression::CountSolutions { .. }
                            | AggregateExpression::FunctionCall {
                                expr: Expression::Variable(_),
                                ..
                            }
                    )
                }) =>
                {
                    pending.push(inner);
                }
                QueryExpression::OrderBy { inner, expression }
                    if expression.iter().all(|order| {
                        matches!(
                            order,
                            OrderExpression::Asc(Expression::Variable(_))
                                | OrderExpression::Desc(Expression::Variable(_))
                        )
                    }) =>
                {
                    pending.push(inner);
                }
                QueryExpression::Join { left, right } => pending.extend([left.as_mut(), right]),
                QueryExpression::Bgp { patterns } => {
                    for triple in patterns {
                        replace(&mut triple.subject, &selected.seed, &replacement);
                        replace(&mut triple.object, &selected.seed, &replacement);
                    }
                }
                QueryExpression::Path {
                    subject, object, ..
                } => {
                    replace(subject, &selected.seed, &replacement);
                    replace(object, &selected.seed, &replacement);
                }
                _ => return Err(dispatch_failure()),
            }
        }
        Ok(query.to_string())
    }
}

fn replace(term: &mut TermPattern, selected: &SymbolId, local: &NamedNode) {
    if let TermPattern::NamedNode(node) = term
        && node.as_str() == selected.as_str()
    {
        node.clone_from(local);
    }
}

fn dispatch_failure() -> RiftError {
    errors::search::query_feature_refused()
        .feature("a graph query without a dispatchable project seed")
        .error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sparql::GraphLimits;

    #[test]
    fn dispatch_refuses_expressions_that_can_observe_backend_identity() {
        let public = "rift://symbol/local:docs/rust/beacon";
        let local = SymbolId::parse("rift://symbol/local/rust/beacon").expect("local identity");
        let statement = format!(
            "SELECT ?caller {{ ?caller :references <{public}> }} ORDER BY CONTAINS(STR(?caller), 'local:docs')"
        );
        let query = GraphQuery::parse(&statement, &GraphLimits::default()).expect("valid query");
        assert!(query.adjacency_selection().is_some());
        assert!(query.dispatched_adjacency(&local).is_err());
    }

    #[test]
    fn dispatch_changes_only_the_anchor_iri() {
        let public = "rift://symbol/local:docs/rust/beacon";
        let local = SymbolId::parse("rift://symbol/local/rust/beacon").expect("local identity");
        let statement = format!("SELECT ?caller {{ ?caller :references <{public}> }}");
        let original = GraphQuery::parse(&statement, &GraphLimits::default()).expect("valid query");
        let dispatched = original
            .dispatched_adjacency(&local)
            .expect("dispatched query");
        let dispatched =
            GraphQuery::parse(&dispatched, &GraphLimits::default()).expect("valid query");
        assert_eq!(
            dispatched.adjacency_selection().expect("adjacency").seed,
            local
        );
        assert_eq!(
            original
                .adjacency_selection()
                .expect("adjacency")
                .seed
                .as_str(),
            public
        );
    }
}
