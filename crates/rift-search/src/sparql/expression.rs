//! The bound on string values a query computes.
//!
//! The answer byte bound counts output, so a value built and then dropped, such as a chain of
//! `BIND(CONCAT(?x, ?x) AS ?y)` doubling one string per step, would allocate past it first.
//! The string functions whose result can outgrow their arguments are replaced, under their own
//! IRIs, by functions that compute the most bytes the call can produce before calling the
//! built-in, and `GROUP_CONCAT` by an accumulator that counts as it appends. Every other
//! function's result is no longer than its arguments.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use oxrdf::vocab::{rdf, xsd};
use oxrdf::{Literal, NamedNode, Term, Variable};
use spareval::{AggregateFunctionAccumulator, CancellationToken, QueryEvaluator};
use spargebra::vocab::sparql;
use sparopt::algebra::Expression;

use super::limits::GraphLimits;

/// IRI prefix the admitted query renames each `GROUP_CONCAT` to, followed by its index.
pub(super) const GROUP_CONCAT_PREFIX: &str = "urn:rift:query:groupConcat:";

/// How many bytes one growing function can produce from its arguments.
#[derive(Clone, Copy)]
enum Growth {
    /// The sum of every argument, as `CONCAT` joins them.
    Sum,
    /// One replacement per character boundary of the text, as `REPLACE` writes at most.
    Replace,
    /// Three bytes per byte of the first argument, the widest escape or case mapping.
    Triple,
}

/// The growing string functions, each with its growth.
const GROWING_FUNCTIONS: [(NamedNode, Growth); 5] = [
    (sparql::CONCAT, Growth::Sum),
    (sparql::REPLACE, Growth::Replace),
    (sparql::ENCODE_FOR_URI, Growth::Triple),
    (sparql::UCASE, Growth::Triple),
    (sparql::LCASE, Growth::Triple),
];

/// The string byte bounds of one evaluation: each computed value, and every computed value
/// together, since many rows of values each within the first bound still add up. Crossing
/// either cancels the evaluation's token and leaves the crossing recorded for the refusal.
#[derive(Clone)]
pub(super) struct ExpressionBudget {
    bytes_max: usize,
    computed_max: usize,
    computed: Arc<AtomicUsize>,
    crossing: Arc<AtomicU8>,
    token: CancellationToken,
}

/// No bound was crossed.
const UNCROSSED: u8 = 0;
/// One value would have passed the value bound.
const VALUE_CROSSED: u8 = 1;
/// The values together would have passed the computed bound.
const COMPUTED_CROSSED: u8 = 2;

impl ExpressionBudget {
    pub(super) fn new(limits: &GraphLimits, token: CancellationToken) -> Self {
        Self {
            bytes_max: limits.expression_bytes_max(),
            computed_max: limits.computed_bytes_max(),
            computed: Arc::new(AtomicUsize::new(0)),
            crossing: Arc::new(AtomicU8::new(UNCROSSED)),
            token,
        }
    }

    /// The bound a value crossed and its limit, once one did.
    pub(super) fn crossing(&self) -> Option<(&'static str, usize)> {
        match self.crossing.load(Ordering::Relaxed) {
            VALUE_CROSSED => Some(("expression bytes", self.bytes_max)),
            COMPUTED_CROSSED => Some(("computed string bytes", self.computed_max)),
            _ => None,
        }
    }

    /// Registers the bounded functions, and one bounded `GROUP_CONCAT` per separator.
    pub(super) fn bound(&self, evaluator: QueryEvaluator, separators: &[String]) -> QueryEvaluator {
        let evaluator =
            GROWING_FUNCTIONS
                .into_iter()
                .fold(evaluator, |evaluator, (name, growth)| {
                    let budget = self.clone();
                    let function = name.clone();
                    evaluator.with_custom_function(name, move |arguments| {
                        budget.call(&function, growth, arguments)
                    })
                });
        separators
            .iter()
            .enumerate()
            .fold(evaluator, |evaluator, (index, separator)| {
                let budget = self.clone();
                let separator: Arc<str> = Arc::from(separator.as_str());
                let name = NamedNode::new_unchecked(format!("{GROUP_CONCAT_PREFIX}{index}"));
                evaluator.with_custom_aggregate_function(name, move || {
                    Box::new(GroupConcat {
                        budget: budget.clone(),
                        separator: Arc::clone(&separator),
                        concat: Some(String::new()),
                        started: false,
                    })
                })
            })
    }

    /// Calls the built-in `function` when its most bytes fit both bounds, charging them to the
    /// evaluation's total first, through an evaluator without overrides.
    fn call(&self, function: &NamedNode, growth: Growth, arguments: &[Term]) -> Option<Term> {
        let bytes = growth.bytes_max(arguments);
        if !self.admits(bytes, bytes) {
            return None;
        }
        let variables: Vec<Variable> = (0..arguments.len())
            .map(|index| Variable::new_unchecked(format!("argument{index}")))
            .collect();
        let call = Expression::FunctionCall(
            function.clone(),
            variables
                .iter()
                .cloned()
                .map(Expression::Variable)
                .collect(),
        );
        let substitutions = variables.iter().zip(arguments.iter().cloned());
        QueryEvaluator::new().evaluate_expression(&call, substitutions)
    }

    /// Admits a value of `value` bytes that adds `added` bytes to the evaluation's total, or
    /// records the crossing and cancels the evaluation.
    fn admits(&self, value: usize, added: usize) -> bool {
        let computed = self
            .computed
            .fetch_add(added, Ordering::Relaxed)
            .saturating_add(added);
        let crossing = if value > self.bytes_max {
            VALUE_CROSSED
        } else if computed > self.computed_max {
            COMPUTED_CROSSED
        } else {
            return true;
        };
        // The first crossing names the refusal.
        let _ = self.crossing.compare_exchange(
            UNCROSSED,
            crossing,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        self.token.cancel();
        false
    }
}

impl Growth {
    fn bytes_max(self, arguments: &[Term]) -> usize {
        let length = |index: usize| arguments.get(index).map_or(0, term_bytes);
        match self {
            Self::Sum => arguments
                .iter()
                .map(term_bytes)
                .fold(0, usize::saturating_add),
            Self::Replace => {
                let boundaries = length(0).saturating_add(1);
                boundaries
                    .saturating_mul(length(2))
                    .saturating_add(length(0))
            }
            Self::Triple => length(0).saturating_mul(3),
        }
    }
}

/// `GROUP_CONCAT` with the separator its query named, counting bytes as it appends.
struct GroupConcat {
    budget: ExpressionBudget,
    separator: Arc<str>,
    concat: Option<String>,
    started: bool,
}

impl AggregateFunctionAccumulator for GroupConcat {
    fn accumulate(&mut self, element: Term) {
        let Some(concat) = self.concat.as_mut() else {
            return;
        };
        let value = match &element {
            Term::Literal(literal) if is_string(literal) => literal.value(),
            Term::Literal(_) | Term::NamedNode(_) | Term::BlankNode(_) => {
                self.concat = None;
                return;
            }
        };
        let separator = if self.started { &*self.separator } else { "" };
        let added = separator.len().saturating_add(value.len());
        if !self
            .budget
            .admits(concat.len().saturating_add(added), added)
        {
            self.concat = None;
            return;
        }
        concat.push_str(separator);
        concat.push_str(value);
        self.started = true;
    }

    fn finish(&mut self) -> Option<Term> {
        self.concat
            .take()
            .map(|concat| Literal::new_simple_literal(concat).into())
    }
}

/// Whether `literal` is a simple or language-tagged string, the values `GROUP_CONCAT` joins.
fn is_string(literal: &Literal) -> bool {
    let datatype = literal.datatype();
    *datatype == xsd::STRING || *datatype == rdf::LANG_STRING
}

/// The bytes of text one term carries.
fn term_bytes(term: &Term) -> usize {
    match term {
        Term::NamedNode(iri) => iri.as_str().len(),
        Term::BlankNode(node) => node.as_str().len(),
        Term::Literal(literal) => literal.value().len(),
    }
}
