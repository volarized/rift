//! Evaluation of one admitted query over one dataset, within its answer and time bounds.

use std::cell::Cell;
use std::convert::Infallible;
use std::time::Instant;

use oxrdf::{Dataset, Term};
use rift_error::{RiftError, errors};
use spareval::{
    CancellationToken, InternalQuad, QueryEvaluationError, QueryEvaluator, QueryResults,
    QuerySolutionIter, QueryableDataset,
};

use super::dataset::GraphDataset;
use super::limits::GraphLimits;
use super::query::GraphQuery;

/// Cancellation one evaluation observes. Cancelling it ends the evaluation at its next dataset
/// read or solution with `query_cancelled`; the evaluation deadline cancels it too, so one
/// token serves one evaluation.
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
    /// The deadline starts at the call and is checked at every dataset read and every
    /// solution: crossing it cancels `cancellation`, which stops spareval's joins at their next
    /// check as well. Evaluation is synchronous CPU work; an async caller runs it on its bounded
    /// blocking pool and may cancel the token from a timer of its own.
    ///
    /// # Errors
    ///
    /// - `query_answer_limit` when the answer needs more solutions or term bytes than allowed.
    /// - `query_deadline_exceeded` when evaluation outlasts the evaluation timeout.
    /// - `query_cancelled` when `cancellation` was cancelled by its holder.
    /// - `query_evaluation_failed` for every other evaluation failure, such as an unknown
    ///   function.
    pub fn evaluate(
        &self,
        query: &GraphQuery,
        cancellation: &QueryCancellation,
    ) -> Result<GraphAnswer, RiftError> {
        let expired = Cell::new(false);
        let watched = DeadlineDataset {
            dataset: self.triples(),
            deadline: Deadline::after(self.limits()),
            cancellation: cancellation.0.clone(),
            expired: &expired,
        };
        let evaluator = QueryEvaluator::new().with_cancellation_token(cancellation.0.clone());
        let failure = |error| evaluation_failure(error, &expired, self.limits());
        let results = evaluator
            .prepare(query.query())
            .execute(watched.clone())
            .map_err(failure)?;
        match results {
            QueryResults::Boolean(value) => Ok(GraphAnswer::Boolean(value)),
            QueryResults::Solutions(solutions) => {
                collect_solutions(solutions, &watched, self.limits(), failure)
                    .map(GraphAnswer::Solutions)
            }
            QueryResults::Graph(_) => errors::search::query_feature_refused()
                .feature("CONSTRUCT")
                .fail(),
        }
    }
}

/// Collects at most the solution and term byte bounds, checking the deadline per solution.
fn collect_solutions(
    solutions: QuerySolutionIter<'_>,
    watched: &DeadlineDataset<'_>,
    limits: &GraphLimits,
    failure: impl Fn(QueryEvaluationError) -> RiftError,
) -> Result<SolutionTable, RiftError> {
    let variables = solutions
        .variables()
        .iter()
        .map(|variable| variable.as_str().to_owned())
        .collect();
    let mut rows = Vec::new();
    let mut answer_bytes = 0_usize;
    for solution in solutions {
        watched.observe_deadline();
        if watched.expired.get() {
            return Err(failure(QueryEvaluationError::Cancelled));
        }
        let solution = solution.map_err(&failure)?;
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
            return errors::search::query_answer_limit()
                .bound(bound)
                .limit(limit)
                .fail();
        }
        rows.push(row);
    }
    Ok(SolutionTable { variables, rows })
}

/// The registered refusal one spareval failure maps to.
fn evaluation_failure(
    error: QueryEvaluationError,
    expired: &Cell<bool>,
    limits: &GraphLimits,
) -> RiftError {
    match error {
        QueryEvaluationError::Cancelled if expired.get() => {
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

/// The dataset as spareval reads it, cancelling evaluation once the deadline passes.
#[derive(Clone)]
struct DeadlineDataset<'a> {
    dataset: &'a Dataset,
    deadline: Deadline,
    cancellation: CancellationToken,
    expired: &'a Cell<bool>,
}

impl DeadlineDataset<'_> {
    /// Cancels evaluation, once, when the deadline has passed.
    fn observe_deadline(&self) {
        if !self.expired.get() && self.deadline.has_passed() {
            self.expired.set(true);
            self.cancellation.cancel();
        }
    }
}

impl<'a> QueryableDataset<'a> for DeadlineDataset<'a> {
    type InternalTerm = Term;
    type Error = Infallible;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&Term>,
        object: Option<&Term>,
        graph_name: Option<Option<&Term>>,
    ) -> impl Iterator<Item = Result<InternalQuad<Term>, Infallible>> + use<'a> {
        self.observe_deadline();
        let watched = self.clone();
        self.dataset
            .internal_quads_for_pattern(subject, predicate, object, graph_name)
            .inspect(move |_| watched.observe_deadline())
    }

    fn internalize_term(&self, term: Term) -> Result<Term, Infallible> {
        Ok(term)
    }

    fn externalize_term(&self, term: Term) -> Result<Term, Infallible> {
        Ok(term)
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
