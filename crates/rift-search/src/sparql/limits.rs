//! Bounds on one graph query: its text, its dataset, its answer, and its evaluation time.

use std::time::Duration;

use rift_error::{RiftError, errors};

/// Default most bytes of query text accepted, 64 KiB.
pub const QUERY_BYTES_MAX: usize = 64 * 1024;
/// Default most triples one dataset materializes. An in-memory dataset indexes each triple
/// six ways, so 200,000 triples hold tens of MiB.
pub const TRIPLES_MAX: usize = 200_000;
/// Default most bytes of literal text one dataset holds, 32 MiB: node source dominates it.
pub const LITERAL_BYTES_MAX: usize = 32 * 1024 * 1024;
/// Default most solutions one answer carries, the largest page a `search` serves.
pub const SOLUTIONS_MAX: usize = 10_000;
/// Default most bytes of term text one answer carries, 16 MiB.
pub const ANSWER_BYTES_MAX: usize = 16 * 1024 * 1024;
/// Default evaluation time before the query is cancelled.
pub const EVALUATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Every bound one graph query is evaluated within. Each crossing is a typed refusal, never a
/// silently shortened answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphLimits {
    query_bytes_max: usize,
    triples_max: usize,
    literal_bytes_max: usize,
    solutions_max: usize,
    answer_bytes_max: usize,
    evaluation_timeout: Duration,
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            query_bytes_max: QUERY_BYTES_MAX,
            triples_max: TRIPLES_MAX,
            literal_bytes_max: LITERAL_BYTES_MAX,
            solutions_max: SOLUTIONS_MAX,
            answer_bytes_max: ANSWER_BYTES_MAX,
            evaluation_timeout: EVALUATION_TIMEOUT,
        }
    }
}

impl GraphLimits {
    /// Starts from the default bounds.
    pub fn builder() -> GraphLimitsBuilder {
        GraphLimitsBuilder {
            limits: Self::default(),
        }
    }

    /// Most bytes of query text.
    #[must_use]
    pub const fn query_bytes_max(&self) -> usize {
        self.query_bytes_max
    }

    /// Most triples one dataset materializes.
    #[must_use]
    pub const fn triples_max(&self) -> usize {
        self.triples_max
    }

    /// Most bytes of literal text one dataset holds.
    #[must_use]
    pub const fn literal_bytes_max(&self) -> usize {
        self.literal_bytes_max
    }

    /// Most solutions one answer carries.
    #[must_use]
    pub const fn solutions_max(&self) -> usize {
        self.solutions_max
    }

    /// Most bytes of term text one answer carries.
    #[must_use]
    pub const fn answer_bytes_max(&self) -> usize {
        self.answer_bytes_max
    }

    /// Evaluation time before the query is cancelled.
    #[must_use]
    pub const fn evaluation_timeout(&self) -> Duration {
        self.evaluation_timeout
    }
}

/// Builds [`GraphLimits`] from the defaults, refusing a zero bound once in [`Self::build`].
#[derive(Debug)]
#[must_use]
pub struct GraphLimitsBuilder {
    limits: GraphLimits,
}

impl GraphLimitsBuilder {
    /// Sets the most bytes of query text.
    pub const fn query_bytes_max(mut self, value: usize) -> Self {
        self.limits.query_bytes_max = value;
        self
    }

    /// Sets the most triples one dataset materializes.
    pub const fn triples_max(mut self, value: usize) -> Self {
        self.limits.triples_max = value;
        self
    }

    /// Sets the most bytes of literal text one dataset holds.
    pub const fn literal_bytes_max(mut self, value: usize) -> Self {
        self.limits.literal_bytes_max = value;
        self
    }

    /// Sets the most solutions one answer carries.
    pub const fn solutions_max(mut self, value: usize) -> Self {
        self.limits.solutions_max = value;
        self
    }

    /// Sets the most bytes of term text one answer carries.
    pub const fn answer_bytes_max(mut self, value: usize) -> Self {
        self.limits.answer_bytes_max = value;
        self
    }

    /// Sets the evaluation time before the query is cancelled.
    pub const fn evaluation_timeout(mut self, value: Duration) -> Self {
        self.limits.evaluation_timeout = value;
        self
    }

    /// Accepts the bounds.
    ///
    /// # Errors
    ///
    /// Returns `graph_limit_zero` naming the first count bound set to zero. A zero evaluation
    /// timeout is accepted: it cancels at the first dataset read or solution.
    pub fn build(self) -> Result<GraphLimits, RiftError> {
        let limits = self.limits;
        let counts = [
            ("query_bytes_max", limits.query_bytes_max),
            ("triples_max", limits.triples_max),
            ("literal_bytes_max", limits.literal_bytes_max),
            ("solutions_max", limits.solutions_max),
            ("answer_bytes_max", limits.answer_bytes_max),
        ];
        match counts.iter().find(|(_, value)| *value == 0) {
            Some((bound, _)) => errors::search::graph_limit_zero().bound(*bound).fail(),
            None => Ok(limits),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GraphLimits;
    use std::time::Duration;

    #[test]
    fn test_graph_limits_builder_sets_every_bound() {
        let limits = GraphLimits::builder()
            .query_bytes_max(1)
            .triples_max(2)
            .literal_bytes_max(3)
            .solutions_max(4)
            .answer_bytes_max(5)
            .evaluation_timeout(Duration::from_millis(6))
            .build()
            .expect("nonzero bounds are accepted");
        assert_eq!(
            (
                limits.query_bytes_max(),
                limits.triples_max(),
                limits.literal_bytes_max(),
                limits.solutions_max(),
                limits.answer_bytes_max(),
                limits.evaluation_timeout(),
            ),
            (1, 2, 3, 4, 5, Duration::from_millis(6))
        );
        assert_eq!(
            GraphLimits::builder().build().ok(),
            Some(GraphLimits::default())
        );
    }

    #[test]
    fn test_graph_limits_builder_refuses_zero_bound() {
        let error = GraphLimits::builder()
            .solutions_max(0)
            .build()
            .expect_err("a zero bound is refused");
        assert_eq!(
            error.slug(),
            rift_error::errors::search::graph_limit_zero::SLUG
        );
        assert!(error.to_string().contains("solutions_max"), "{error}");
    }
}
