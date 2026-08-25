//! Cosine ranking of one query vector against the vectors a store holds.
//!
//! The encoder returns unit-length vectors, so a bare dot product would answer
//! for everything it produced. A row read back from storage is not proof that
//! the encoder produced it: the blob is decoded values, and a row of zeros
//! divides a dot product by nothing at all. Dividing by both magnitudes costs
//! one pass over each vector, keeps the score a cosine whatever the row holds,
//! and gives a zero-magnitude row a score of 0 rather than a value that is not
//! a number.
//!
//! The scan is sans-I/O: the corpus arrives as values the caller already read.

use std::cmp::Ordering;

use rayon::prelude::*;
use rift_index::StoredVector;

use crate::error::{SearchError, SearchFault, SearchViolation};

/// One vector the semantic ranking returned, addressed by the digest of the
/// text it was built from.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticMatch {
    digest: String,
    similarity: f32,
}

impl SemanticMatch {
    /// Constructs one semantic match directly. Production code only ever builds
    /// these from [`nearest`]; this constructor exists for callers that carry a
    /// match they already hold into another ranking.
    #[must_use]
    pub const fn new(digest: String, similarity: f32) -> Self {
        Self { digest, similarity }
    }

    /// The digest of the text this vector was embedded from.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The cosine of the angle between this vector and the query.
    #[must_use]
    pub const fn similarity(&self) -> f32 {
        self.similarity
    }
}

/// Ranks `corpus` against `query` by cosine similarity, best first, keeping at
/// most `keep_max`.
///
/// Scoring is one pass over the corpus, spread across the rayon pool:
/// `corpus.len() * query.len()` multiplications, and no allocation beyond one
/// match per row. The head is then partitioned in linear time and only the head
/// is sorted, so keeping `k` of `n` rows costs `O(n + k log k)` rather than
/// sorting all of `n`. Neither loop carries a budget: the corpus is a slice
/// whose length is already the caller's own bound, and every row is scored.
///
/// The order is descending similarity, ties broken by digest ascending, so one
/// query and one corpus always produce one order however the pool schedules the
/// scan.
///
/// # Errors
///
/// Returns `vector_width_mismatch` naming the query's width and the width it
/// met when `query` is empty, or when a corpus row is not as wide as the query.
/// A vector of another width is noise rather than an answer, and the refusal
/// lands before any row is scored.
pub fn nearest(
    query: &[f32],
    corpus: &[StoredVector],
    keep_max: usize,
) -> Result<Vec<SemanticMatch>, SearchError> {
    if let Some((query_width, stored_width)) = width_refusal(query, corpus) {
        return Err(SearchError::new(
            SearchFault::new(SearchViolation::VectorWidthMismatch).about(format!(
                "query width {query_width}, stored width {stored_width}"
            )),
        ));
    }
    if keep_max == 0 {
        return Ok(Vec::new());
    }
    let query_magnitude = magnitude(query);
    let mut scored: Vec<SemanticMatch> = corpus
        .par_iter()
        .map(|stored| SemanticMatch {
            digest: stored.digest().to_owned(),
            similarity: cosine(query, stored.values(), query_magnitude),
        })
        .collect();
    let head = keep_max.min(scored.len());
    if head < scored.len() {
        scored.select_nth_unstable_by(head, nearest_first);
        scored.truncate(head);
    }
    scored.sort_unstable_by(nearest_first);
    Ok(scored)
}

/// The first width that cannot be scored against the query, as the pair a
/// refusal names.
///
/// An empty query is reported against the first stored width, which is the
/// width the caller meant to ask in. The scan over the corpus stops at the
/// first row that differs and is otherwise bounded by the slice the caller
/// handed over.
fn width_refusal(query: &[f32], corpus: &[StoredVector]) -> Option<(usize, usize)> {
    let stored_width = corpus.first().map_or(0, |stored| stored.values().len());
    if query.is_empty() {
        return Some((0, stored_width));
    }
    corpus
        .iter()
        .map(|stored| stored.values().len())
        .find(|width| *width != query.len())
        .map(|width| (query.len(), width))
}

/// The cosine of the angle between two vectors of one width.
///
/// A magnitude of zero scores 0: a row of zeros has no direction, and dividing
/// by it produces a value that is not a number, which no ordering can place.
fn cosine(query: &[f32], values: &[f32], query_magnitude: f32) -> f32 {
    let divisor = query_magnitude * magnitude(values);
    if divisor <= 0.0 {
        return 0.0;
    }
    let dot: f32 = query
        .iter()
        .zip(values)
        .map(|(one, other)| one * other)
        .sum();
    dot / divisor
}

/// One vector's length.
fn magnitude(values: &[f32]) -> f32 {
    values.iter().map(|value| value * value).sum::<f32>().sqrt()
}

/// Descending similarity, then digest ascending.
///
/// The comparison is total: `f32::total_cmp` orders every value a stored row
/// can hold, so a partition cannot depend on which rows the pool compared
/// first.
fn nearest_first(one: &SemanticMatch, other: &SemanticMatch) -> Ordering {
    other
        .similarity
        .total_cmp(&one.similarity)
        .then_with(|| one.digest.cmp(&other.digest))
}

#[cfg(test)]
mod tests {
    use super::{SemanticMatch, cosine, magnitude, nearest_first, width_refusal};
    use rift_index::StoredVector;
    use std::cmp::Ordering;

    /// Whether two similarities agree to single-precision accuracy. Cosine
    /// divides, so the two sides differ by the rounding of that division.
    fn is_similarity(computed: f32, expected: f32) -> bool {
        (computed - expected).abs() < 1e-6
    }

    fn stored(digest: &str, values: &[f32]) -> StoredVector {
        StoredVector::new(digest.to_owned(), values.to_vec())
    }

    #[test]
    fn test_cosine_divides_by_both_magnitudes() {
        let query = [3.0, 0.0];
        let magnitude_of_query = magnitude(&query);
        assert!(is_similarity(magnitude_of_query, 3.0));
        assert!(
            is_similarity(cosine(&query, &[5.0, 0.0], magnitude_of_query), 1.0),
            "a vector of another length points the same way"
        );
        assert!(is_similarity(
            cosine(&query, &[0.0, 7.0], magnitude_of_query),
            0.0
        ));
        assert!(is_similarity(
            cosine(&query, &[-2.0, 0.0], magnitude_of_query),
            -1.0
        ));
    }

    #[test]
    fn test_a_vector_without_direction_scores_zero() {
        let query = [1.0, 0.0];
        let magnitude_of_query = magnitude(&query);
        let scored = cosine(&query, &[0.0, 0.0], magnitude_of_query);
        assert!(scored.is_finite(), "a zero magnitude must not divide");
        assert!(is_similarity(scored, 0.0));
        assert!(is_similarity(cosine(&[0.0, 0.0], &[1.0, 0.0], 0.0), 0.0));
        assert!(is_similarity(magnitude(&[]), 0.0));
    }

    #[test]
    fn test_the_order_is_similarity_then_digest() {
        let strong = SemanticMatch::new("bbb".to_owned(), 0.9);
        let weak = SemanticMatch::new("aaa".to_owned(), 0.1);
        assert_eq!(nearest_first(&strong, &weak), Ordering::Less);
        assert_eq!(nearest_first(&weak, &strong), Ordering::Greater);
        let tied = SemanticMatch::new("ccc".to_owned(), 0.9);
        assert_eq!(nearest_first(&strong, &tied), Ordering::Less);
        let same = SemanticMatch::new("bbb".to_owned(), 0.9);
        assert_eq!(nearest_first(&strong, &same), Ordering::Equal);
    }

    #[test]
    fn test_width_refusal_names_the_query_and_the_row_it_met() {
        let corpus = [stored("aaa", &[1.0, 0.0]), stored("bbb", &[1.0])];
        assert_eq!(width_refusal(&[1.0, 0.0], &corpus[..1]), None);
        assert_eq!(width_refusal(&[1.0, 0.0], &corpus), Some((2, 1)));
        assert_eq!(
            width_refusal(&[], &corpus),
            Some((0, 2)),
            "an empty query is reported against the width the caller meant"
        );
        assert_eq!(width_refusal(&[], &[]), Some((0, 0)));
        assert_eq!(width_refusal(&[1.0], &[]), None, "an empty corpus fits");
    }

    #[test]
    fn test_a_match_reports_its_digest_and_similarity() {
        let matched = SemanticMatch::new("aaa".to_owned(), 0.5);
        assert_eq!(matched.digest(), "aaa");
        assert!(is_similarity(matched.similarity(), 0.5));
    }
}
