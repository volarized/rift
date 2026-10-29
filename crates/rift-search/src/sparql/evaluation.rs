//! Evaluation of one admitted query over one dataset, within its answer and time bounds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use oxrdf::Term;
use rift_error::{RiftError, errors};
use spareval::{
    CancellationToken, QueryEvaluationError, QueryEvaluator, QueryResults, QuerySolutionIter,
};

use super::dataset::GraphDataset;
use super::expression::ExpressionBudget;
use super::limits::GraphLimits;
use super::query::GraphQuery;
use super::query_thread::{self, Watch};

/// Cancellation one evaluation observes. Cancelling it ends the evaluation at spareval's next
/// check with `query_cancelled`; the evaluation deadline and the expression byte bound cancel
/// it too, so one token serves one evaluation.
#[derive(Clone, Default)]
pub struct QueryCancellation(CancellationToken);

impl QueryCancellation {
    /// A token no one has cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels every evaluation holding this token.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Whether the token is cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

impl std::fmt::Debug for QueryCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("QueryCancellation")
            .field(&self.is_cancelled())
            .finish()
    }
}

/// One RDF term of an answer.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum GraphTerm {
    /// An IRI, such as a symbol identity.
    Iri(String),
    /// A blank node label, stable within one answer.
    BlankNode(String),
    /// A literal.
    Literal {
        /// The lexical form.
        value: String,
        /// The datatype IRI; `rdf:langString` for a language-tagged string.
        datatype: String,
        /// The language tag, when present.
        language: Option<String>,
    },
}

/// The rows of a `SELECT` answer, in evaluation order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SolutionTable {
    /// The projected variable names, without `?`.
    pub variables: Vec<String>,
    /// One entry per solution, one value per variable; `None` leaves the variable unbound.
    pub rows: Vec<Vec<Option<GraphTerm>>>,
}

/// The answer to one admitted query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphAnswer {
    /// The solutions of a `SELECT`.
    Solutions(SolutionTable),
    /// The truth of an `ASK`.
    Boolean(bool),
}

impl GraphDataset {
    /// Evaluates `query` within this dataset's bounds.
    ///
    /// Evaluation runs on the query thread while the calling thread holds the deadline: at the
    /// evaluation timeout it cancels `cancellation`, which spareval checks at every dataset read
    /// and inside its join loops, and every solution is checked against the deadline as well.
    /// The string functions that can outgrow their arguments refuse a value past the expression
    /// byte bound before computing it. The call blocks; an async caller runs it on its bounded
    /// blocking pool.
    ///
    /// # Errors
    ///
    /// - `query_answer_limit` when the answer needs more solutions or term bytes than allowed,
    ///   or one computed string needs more expression bytes.
    /// - `query_deadline_exceeded` when evaluation outlasts the evaluation timeout.
    /// - `query_cancelled` when `cancellation` was cancelled by its holder.
    /// - `query_evaluation_failed` for every other evaluation failure, such as an unknown
    ///   function, and when the query thread cannot start.
    pub fn evaluate(
        &self,
        query: &GraphQuery,
        cancellation: &QueryCancellation,
    ) -> Result<GraphAnswer, RiftError> {
        let limits = self.limits();
        let expired = AtomicBool::new(false);
        let budget = ExpressionBudget::new(limits.expression_bytes_max(), cancellation.0.clone());
        let watch = Watch {
            timeout: limits.evaluation_timeout(),
            token: &cancellation.0,
            expired: &expired,
        };
        let deadline = Deadline::after(limits);
        let outcome = query_thread::run(Some(watch), || {
            self.evaluate_here(query, cancellation, &budget, deadline, &expired)
        })?;
        match outcome {
            Ok(answer) => Ok(answer),
            Err(Failure::Refused(error)) => Err(error),
            Err(Failure::Evaluation(error)) => {
                Err(evaluation_failure(error, &budget, &expired, limits))
            }
        }
    }

    /// Evaluates `query` on the calling thread, leaving cancellation causes to the caller.
    fn evaluate_here(
        &self,
        query: &GraphQuery,
        cancellation: &QueryCancellation,
        budget: &ExpressionBudget,
        deadline: Deadline,
        expired: &AtomicBool,
    ) -> Result<GraphAnswer, Failure> {
        let evaluator = QueryEvaluator::new().with_cancellation_token(cancellation.0.clone());
        let evaluator = budget.bound(evaluator, query.separators());
        let results = evaluator
            .prepare(query.query())
            .execute(self.triples())
            .map_err(Failure::Evaluation)?;
        let checkpoint = Checkpoint {
            budget,
            deadline,
            expired,
        };
        let answer = match results {
            QueryResults::Boolean(value) => GraphAnswer::Boolean(value),
            QueryResults::Solutions(solutions) => {
                GraphAnswer::Solutions(collect_solutions(solutions, &checkpoint, self.limits())?)
            }
            QueryResults::Graph(_) => {
                let refusal = errors::search::query_feature_refused().feature("CONSTRUCT");
                return Err(Failure::Refused(refusal.error()));
            }
        };
        checkpoint.observe()?;
        Ok(answer)
    }
}

/// Why an evaluation stopped before answering.
enum Failure {
    /// spareval failed, cancellation included; the cause is decided after the thread joins.
    Evaluation(QueryEvaluationError),
    /// A bound refused the answer.
    Refused(RiftError),
}

/// The checks one evaluation makes between solutions.
struct Checkpoint<'a> {
    budget: &'a ExpressionBudget,
    deadline: Deadline,
    expired: &'a AtomicBool,
}

impl Checkpoint<'_> {
    /// Fails as cancelled once the deadline passed or a string crossed its bound.
    fn observe(&self) -> Result<(), Failure> {
        if self.deadline.has_passed() {
            self.expired.store(true, Ordering::Relaxed);
        }
        if self.expired.load(Ordering::Relaxed) || self.budget.is_crossed() {
            return Err(Failure::Evaluation(QueryEvaluationError::Cancelled));
        }
        Ok(())
    }
}

/// Collects at most the solution and term byte bounds, observing the checkpoint per solution.
fn collect_solutions(
    solutions: QuerySolutionIter<'_>,
    checkpoint: &Checkpoint<'_>,
    limits: &GraphLimits,
) -> Result<SolutionTable, Failure> {
    let variables = solutions
        .variables()
        .iter()
        .map(|variable| variable.as_str().to_owned())
        .collect();
    let mut rows = Vec::new();
    let mut answer_bytes = 0_usize;
    for solution in solutions {
        checkpoint.observe()?;
        let solution = solution.map_err(Failure::Evaluation)?;
        let row: Vec<Option<GraphTerm>> = solution
            .values()
            .iter()
            .map(|value| value.as_ref().map(GraphTerm::from))
            .collect();
        answer_bytes =
            answer_bytes.saturating_add(row.iter().flatten().map(GraphTerm::text_bytes).sum());
        let crossed = [
            ("solutions", rows.len() + 1, limits.solutions_max()),
            ("answer bytes", answer_bytes, limits.answer_bytes_max()),
        ];
        if let Some((bound, _, limit)) = crossed
            .into_iter()
            .find(|(_, required, limit)| required > limit)
        {
            let refusal = errors::search::query_answer_limit()
                .bound(bound)
                .limit(limit);
            return Err(Failure::Refused(refusal.error()));
        }
        rows.push(row);
    }
    Ok(SolutionTable { variables, rows })
}

/// The registered refusal one spareval failure maps to.
fn evaluation_failure(
    error: QueryEvaluationError,
    budget: &ExpressionBudget,
    expired: &AtomicBool,
    limits: &GraphLimits,
) -> RiftError {
    match error {
        QueryEvaluationError::Cancelled if budget.is_crossed() => {
            errors::search::query_answer_limit()
                .bound("expression bytes")
                .limit(budget.bytes_max())
                .error()
        }
        QueryEvaluationError::Cancelled if expired.load(Ordering::Relaxed) => {
            let timeout_ms =
                u64::try_from(limits.evaluation_timeout().as_millis()).unwrap_or(u64::MAX);
            errors::search::query_deadline_exceeded()
                .timeout_ms(timeout_ms)
                .error()
        }
        QueryEvaluationError::Cancelled => errors::search::query_cancelled().error(),
        error => errors::search::query_evaluation_failed()
            .detail(error.to_string())
            .source(error)
            .error(),
    }
}

/// The instant evaluation must end by; `None` when the timeout passes the clock's range.
#[derive(Clone, Copy, Debug)]
struct Deadline(Option<Instant>);

impl Deadline {
    fn after(limits: &GraphLimits) -> Self {
        Self(Instant::now().checked_add(limits.evaluation_timeout()))
    }

    fn has_passed(self) -> bool {
        self.0.is_some_and(|deadline| Instant::now() >= deadline)
    }
}

impl From<&Term> for GraphTerm {
    fn from(term: &Term) -> Self {
        match term {
            Term::NamedNode(iri) => Self::Iri(iri.as_str().to_owned()),
            Term::BlankNode(node) => Self::BlankNode(node.as_str().to_owned()),
            Term::Literal(literal) => Self::Literal {
                value: literal.value().to_owned(),
                datatype: literal.datatype().as_str().to_owned(),
                language: literal.language().map(str::to_owned),
            },
        }
    }
}

impl GraphTerm {
    /// The bytes of text the term carries, counted against the answer byte bound.
    fn text_bytes(&self) -> usize {
        match self {
            Self::Iri(text) | Self::BlankNode(text) => text.len(),
            Self::Literal {
                value,
                datatype,
                language,
            } => value.len() + datatype.len() + language.as_ref().map_or(0, String::len),
        }
    }
}
