//! Admission of a parsed query: the forms, clauses and joins the graph does not serve.

use std::collections::HashSet;

use oxrdf::NamedNode;
use rift_error::{RiftError, errors};
use spargebra::Query;
use spargebra::algebra::{AggregateExpression, Expression, OrderExpression, QueryExpression};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};
use spargebra::vocab::sparql;

use super::expression::GROUP_CONCAT_PREFIX;
use super::limits::GraphLimits;
use super::query::refused;

/// The feature a join of patterns sharing no variable is refused as.
const CARTESIAN_PRODUCT: &str = "a cartesian product of patterns sharing no variable";

/// One algebra element still to inspect.
enum Visit<'a> {
    Pattern(&'a mut QueryExpression),
    Expression(&'a mut Expression),
}

/// Refuses what the graph does not serve, renames each `GROUP_CONCAT` to its bounded form,
/// and returns the separators those forms take, by index.
///
/// spareval's hash join checks no cancellation between the rows it joins in memory, so rows
/// written inline would join past the deadline: the product of every `VALUES` table's row count
/// is refused past the solution bound. Rows read from the dataset reach the token at every read.
///
/// The walk keeps an explicit stack, so algebra depth never deepens the call stack. Its work
/// is one step per algebra element, bounded by the query byte bound the parser ran under,
/// plus one in-scope variable scan of both sides of every join.
pub(super) fn admit(query: &mut Query, limits: &GraphLimits) -> Result<Vec<String>, RiftError> {
    let root = match query {
        Query::Select(select) if select.dataset.is_none() => &mut select.expression,
        Query::Ask(ask) if ask.dataset.is_none() => &mut ask.expression,
        Query::Select(_) | Query::Ask(_) => return refused("a FROM or FROM NAMED clause"),
        Query::Construct(_) => return refused("CONSTRUCT"),
        Query::Describe(_) => return refused("DESCRIBE"),
    };
    let mut separators = Vec::new();
    let mut joined = Joined::default();
    let mut pending = Pending::from(Visit::Pattern(root));
    while let Some(visit) = pending.next(limits)? {
        match visit {
            Visit::Pattern(pattern) => {
                joined.count(pattern);
                admit_pattern(pattern, &mut separators, &mut pending)?;
            }
            Visit::Expression(expression) => push_expression_children(expression, &mut pending),
        }
    }
    joined.admit(limits)?;
    Ok(separators)
}

/// The algebra elements still to inspect, each with its depth below the root.
struct Pending<'a> {
    stack: Vec<(Visit<'a>, usize)>,
    depth: usize,
}

impl<'a> From<Visit<'a>> for Pending<'a> {
    fn from(root: Visit<'a>) -> Self {
        Self {
            stack: vec![(root, 0)],
            depth: 0,
        }
    }
}

impl<'a> Pending<'a> {
    /// The next element, or a refusal once one lies deeper than the planner and evaluator may
    /// recurse: both recurse once per level, and the text bounds bound the depth only as far
    /// as the parser and this scan agree on what the text holds.
    fn next(&mut self, limits: &GraphLimits) -> Result<Option<Visit<'a>>, RiftError> {
        let Some((visit, depth)) = self.stack.pop() else {
            return Ok(None);
        };
        if depth > limits.algebra_depth_max() {
            return errors::search::query_structure_limit()
                .bound("algebra levels")
                .count(depth)
                .limit(limits.algebra_depth_max())
                .fail();
        }
        self.depth = depth;
        Ok(Some(visit))
    }

    /// Queues a child of the element last returned.
    fn push(&mut self, visit: Visit<'a>) {
        self.stack.push((visit, self.depth + 1));
    }

    /// Queues children of the element last returned.
    fn extend(&mut self, visits: impl IntoIterator<Item = Visit<'a>>) {
        let depth = self.depth + 1;
        self.stack
            .extend(visits.into_iter().map(|visit| (visit, depth)));
    }
}

/// What one query joins: its leaf patterns, and the product of its inline rows.
struct Joined {
    patterns: usize,
    inline_rows: usize,
}

impl Default for Joined {
    fn default() -> Self {
        Self {
            patterns: 0,
            inline_rows: 1,
        }
    }
}

impl Joined {
    /// Counts the triple, path and `VALUES` patterns `pattern` holds itself.
    fn count(&mut self, pattern: &QueryExpression) {
        match pattern {
            QueryExpression::Bgp { patterns } => {
                self.patterns = self.patterns.saturating_add(patterns.len());
            }
            QueryExpression::Path { .. } => self.patterns = self.patterns.saturating_add(1),
            QueryExpression::Values { bindings, .. } => {
                self.patterns = self.patterns.saturating_add(1);
                self.inline_rows = self.inline_rows.saturating_mul(bindings.len().max(1));
            }
            _ => {}
        }
    }

    /// Refuses more patterns than the planner may order, or inline rows joining past the
    /// solution bound.
    fn admit(&self, limits: &GraphLimits) -> Result<(), RiftError> {
        if self.patterns > limits.patterns_max() {
            return errors::search::query_structure_limit()
                .bound("patterns")
                .count(self.patterns)
                .limit(limits.patterns_max())
                .fail();
        }
        if self.inline_rows > limits.solutions_max() {
            return errors::search::query_answer_limit()
                .bound("joined VALUES rows")
                .limit(limits.solutions_max())
                .fail();
        }
        Ok(())
    }
}

/// Refuses one pattern the graph does not serve, then pushes what it holds.
fn admit_pattern<'a>(
    pattern: &'a mut QueryExpression,
    separators: &mut Vec<String>,
    pending: &mut Pending<'a>,
) -> Result<(), RiftError> {
    match pattern {
        QueryExpression::Service { name, .. } => return refused(&format!("SERVICE {name}")),
        QueryExpression::Bgp { patterns } if is_disconnected(patterns) => {
            return refused(CARTESIAN_PRODUCT);
        }
        QueryExpression::Join { left, right } | QueryExpression::LeftJoin { left, right, .. }
            if is_product(left, right) =>
        {
            return refused(CARTESIAN_PRODUCT);
        }
        QueryExpression::Group { aggregates, .. } => {
            for (_, aggregate) in aggregates {
                bound_group_concat(aggregate, separators);
            }
        }
        _ => {}
    }
    push_pattern_children(pattern, pending);
    Ok(())
}

/// Renames a `GROUP_CONCAT` to the bounded aggregate taking its separator.
fn bound_group_concat(aggregate: &mut AggregateExpression, separators: &mut Vec<String>) {
    if let AggregateExpression::FunctionCall {
        name, scalarvals, ..
    } = aggregate
        && *name == sparql::AGG_GROUP_CONCAT
    {
        let separator = scalarvals
            .iter()
            .find(|(key, _)| key.as_str() == "separator")
            .map_or(" ", |(_, value)| value.as_str());
        *name = NamedNode::new_unchecked(format!("{GROUP_CONCAT_PREFIX}{}", separators.len()));
        separators.push(separator.to_owned());
    }
}

/// Whether two joined patterns form a cartesian product: both bind variables, they share
/// none, and neither yields at most one row.
fn is_product(left: &QueryExpression, right: &QueryExpression) -> bool {
    let (left_variables, right_variables) = (in_scope(left), in_scope(right));
    let unconstrained = !left_variables.is_empty()
        && !right_variables.is_empty()
        && left_variables.is_disjoint(&right_variables);
    unconstrained && !at_most_one_row(left) && !at_most_one_row(right)
}

fn in_scope(pattern: &QueryExpression) -> HashSet<&str> {
    let mut variables = HashSet::new();
    pattern.on_in_scope_variable(|variable| {
        variables.insert(variable.as_str());
    });
    variables
}

/// Whether `pattern` yields at most one solution whatever the data holds.
fn at_most_one_row(mut pattern: &QueryExpression) -> bool {
    loop {
        pattern = match pattern {
            QueryExpression::Bgp { patterns } => return patterns.is_empty(),
            QueryExpression::Values { bindings, .. } => return bindings.len() <= 1,
            QueryExpression::Group { variables, .. } => return variables.is_empty(),
            QueryExpression::Slice {
                limit: Some(limit), ..
            } if *limit <= 1 => return true,
            QueryExpression::Extend { inner, .. }
            | QueryExpression::Filter { inner, .. }
            | QueryExpression::OrderBy { inner, .. }
            | QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner }
            | QueryExpression::Reduced { inner }
            | QueryExpression::Slice { inner, .. } => inner,
            _ => return false,
        };
    }
}

/// Whether the triple patterns of one basic graph pattern fall into two or more groups that
/// share no variable; the parser already turned blank nodes into variables. The scan
/// compares every pair once per reached pattern.
fn is_disconnected(patterns: &[TriplePattern]) -> bool {
    let terms: Vec<Vec<String>> = patterns.iter().map(pattern_terms).collect();
    let bound: Vec<usize> = (0..terms.len())
        .filter(|index| !terms[*index].is_empty())
        .collect();
    let Some(&first) = bound.first() else {
        return false;
    };
    let mut reached = vec![false; terms.len()];
    reached[first] = true;
    let mut frontier = vec![first];
    while let Some(index) = frontier.pop() {
        reach_neighbors(&terms, &bound, index, &mut reached, &mut frontier);
    }
    bound.iter().any(|index| !reached[*index])
}

/// Marks every bound pattern sharing a term with pattern `index` reached.
fn reach_neighbors(
    terms: &[Vec<String>],
    bound: &[usize],
    index: usize,
    reached: &mut [bool],
    frontier: &mut Vec<usize>,
) {
    let neighbors = bound.iter().filter(|other| {
        !reached[**other]
            && terms[**other]
                .iter()
                .any(|term| terms[index].contains(term))
    });
    let neighbors: Vec<usize> = neighbors.copied().collect();
    for other in neighbors {
        reached[other] = true;
        frontier.push(other);
    }
}

/// The variables one triple pattern binds.
fn pattern_terms(pattern: &TriplePattern) -> Vec<String> {
    let term = |term: &TermPattern| match term {
        TermPattern::Variable(variable) => Some(variable.as_str().to_owned()),
        TermPattern::NamedNode(_) | TermPattern::Literal(_) => None,
    };
    let predicate = match &pattern.predicate {
        NamedNodePattern::Variable(variable) => Some(variable.as_str().to_owned()),
        NamedNodePattern::NamedNode(_) => None,
    };
    [term(&pattern.subject), predicate, term(&pattern.object)]
        .into_iter()
        .flatten()
        .collect()
}

/// Pushes the patterns and expressions one pattern holds.
fn push_pattern_children<'a>(pattern: &'a mut QueryExpression, pending: &mut Pending<'a>) {
    match pattern {
        QueryExpression::Bgp { .. }
        | QueryExpression::Path { .. }
        | QueryExpression::Values { .. }
        | QueryExpression::Service { .. } => {}
        QueryExpression::Join { left, right }
        | QueryExpression::Union { left, right }
        | QueryExpression::Minus { left, right } => {
            pending.extend([Visit::Pattern(left), Visit::Pattern(right)]);
        }
        QueryExpression::LeftJoin {
            left,
            right,
            expression,
        } => {
            pending.extend([Visit::Pattern(left), Visit::Pattern(right)]);
            pending.extend(expression.iter_mut().map(Visit::Expression));
        }
        QueryExpression::Filter { expr, inner } => {
            pending.extend([Visit::Expression(expr), Visit::Pattern(inner)]);
        }
        QueryExpression::Extend {
            inner, expression, ..
        } => pending.extend([Visit::Expression(expression), Visit::Pattern(inner)]),
        QueryExpression::OrderBy { inner, expression } => {
            pending.push(Visit::Pattern(inner));
            pending.extend(expression.iter_mut().map(|order| match order {
                OrderExpression::Asc(expression) | OrderExpression::Desc(expression) => {
                    Visit::Expression(expression)
                }
            }));
        }
        QueryExpression::Group {
            inner, aggregates, ..
        } => {
            pending.push(Visit::Pattern(inner));
            pending.extend(
                aggregates
                    .iter_mut()
                    .filter_map(|(_, aggregate)| match aggregate {
                        AggregateExpression::CountSolutions { .. } => None,
                        AggregateExpression::FunctionCall { expr, .. } => {
                            Some(Visit::Expression(expr))
                        }
                    }),
            );
        }
        QueryExpression::Graph { inner, .. }
        | QueryExpression::Project { inner, .. }
        | QueryExpression::Distinct { inner }
        | QueryExpression::Reduced { inner }
        | QueryExpression::Slice { inner, .. } => pending.push(Visit::Pattern(inner)),
    }
}

/// Pushes the subexpressions and `EXISTS` patterns one expression holds.
fn push_expression_children<'a>(expression: &'a mut Expression, pending: &mut Pending<'a>) {
    match expression {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => {}
        Expression::Or(left, right) | Expression::And(left, right) => {
            pending.extend([Visit::Expression(left), Visit::Expression(right)]);
        }
        Expression::In(needle, haystack) => {
            pending.push(Visit::Expression(needle));
            pending.extend(haystack.iter_mut().map(Visit::Expression));
        }
        Expression::Exists(pattern) => pending.push(Visit::Pattern(pattern)),
        Expression::If(condition, then, otherwise) => pending.extend([
            Visit::Expression(condition),
            Visit::Expression(then),
            Visit::Expression(otherwise),
        ]),
        Expression::Coalesce(arguments) | Expression::FunctionCall(_, arguments) => {
            pending.extend(arguments.iter_mut().map(Visit::Expression));
        }
    }
}
