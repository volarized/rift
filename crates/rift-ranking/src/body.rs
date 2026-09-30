//! Body matches: the declarations inside a matched file row that hold a query's terms.
//!
//! A symbol document carries no source, so a word that sits only inside a declaration's
//! body matches the row holding its file's text. The read path still answers with the
//! declaration: it finds where the query's terms sit in the first file rows of the ranked
//! list, maps each position to its smallest containing declaration, scores every
//! declaration found by BM25 over its own span, and places them together at the first
//! file row's place ([`crate::RankedCandidates::with_body_matches`]).
//!
//! BM25 here runs with `k1` 0.9, `b` 0.4, and a fixed 2,000-byte average declaration
//! length. Known-item search over the Rift, Bun, and Next.js repositories ranked body
//! queries better under these than under FTS5's own 1.2 and 0.75, and every corpus lost
//! relevance when the average was each corpus's measured mean (263 to 546 bytes), which
//! pushed long declarations down.

use std::collections::{BTreeMap, BTreeSet};

use crate::query::ParsedQuery;
use crate::tokenizer::{token_spans, tokenize};

/// File rows at the head of a ranked list whose declarations join one pool.
pub const BODY_MATCH_FILE_ROWS_MAX: usize = 20;

/// BM25 term-frequency saturation for one declaration's term count.
const BODY_MATCH_K1: f64 = 0.9;
/// BM25 length normalization against [`BODY_MATCH_AVERAGE_BYTES`].
const BODY_MATCH_B: f64 = 0.4;
/// The declaration length, in bytes, BM25 normalizes a span against.
const BODY_MATCH_AVERAGE_BYTES: f64 = 2_000.0;
/// The inverse document frequency BM25 substitutes when the computed value would be zero
/// or negative, which happens once a term sits in more than half the rows.
const BODY_MATCH_IDF_FLOOR: f64 = 1e-6;

/// The lowercase terms a body match looks for: every token of every member the parsed
/// query kept, split the way the corpus tokenizer splits.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BodyTerms(BTreeSet<String>);

impl BodyTerms {
    /// The terms of `query`'s members, unquoted terms and quoted phrases alike.
    #[must_use]
    pub fn of(query: &ParsedQuery) -> Self {
        Self(
            query
                .members()
                .iter()
                .flat_map(|member| tokenize(member.text()))
                .collect(),
        )
    }

    /// Whether the query carried no term at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The terms, in lexical order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Every occurrence of a term in `text`, as the byte offset its token starts at and
    /// the term it spells, in text order. Work is one pass over `text`.
    #[must_use]
    pub fn occurrences<'terms>(&'terms self, text: &str) -> Vec<(usize, &'terms str)> {
        token_spans(text)
            .into_iter()
            .filter_map(|(offset, token)| {
                let term = self.0.get(&token.to_lowercase())?;
                Some((offset, term.as_str()))
            })
            .collect()
    }
}

/// How many file rows the store holds, and how many of them hold each term: the document
/// frequencies a declaration's score reads.
///
/// The store counts rows, so a file split into chunks counts once per chunk holding a
/// term; ranks came out identical to counting files on every evaluated corpus.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileRowFrequencies {
    rows: u64,
    by_term: BTreeMap<String, u64>,
}

impl FileRowFrequencies {
    /// Names the row count and each term's row frequency. A term left out holds no row.
    #[must_use]
    pub fn new(rows: u64, by_term: impl IntoIterator<Item = (String, u64)>) -> Self {
        Self {
            rows,
            by_term: by_term.into_iter().collect(),
        }
    }

    /// How many file rows the store holds.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// How many file rows hold `term`.
    #[must_use]
    pub fn rows_holding(&self, term: &str) -> u64 {
        self.by_term.get(term).copied().unwrap_or(0)
    }

    /// BM25's inverse document frequency of `term` over the file rows.
    fn inverse_frequency(&self, term: &str) -> f64 {
        let rows = as_float(self.rows);
        let holding = as_float(self.rows_holding(term));
        ((rows - holding + 0.5) / (holding + 0.5))
            .ln()
            .max(BODY_MATCH_IDF_FLOOR)
    }
}

/// Declarations found in file rows, each with its BM25 score.
///
/// The pool holds one entry per declaration a file row named; a declaration two rows
/// both hold enters twice, and the later entry never moves it, since placement keeps the
/// first place an identity reaches.
#[derive(Clone, Debug)]
pub struct BodyMatchPool<T> {
    scored: Vec<(f64, T)>,
}

impl<T> Default for BodyMatchPool<T> {
    fn default() -> Self {
        Self { scored: Vec::new() }
    }
}

impl<T> BodyMatchPool<T> {
    /// Scores one declaration by the terms it holds, `counts` per term, over a span of
    /// `length_bytes`, and adds it to the pool.
    pub fn add(
        &mut self,
        frequencies: &FileRowFrequencies,
        declaration: T,
        counts: &BTreeMap<&str, u32>,
        length_bytes: u64,
    ) {
        let normalized = as_float(length_bytes) / BODY_MATCH_AVERAGE_BYTES;
        let length_factor = BODY_MATCH_K1 * (1.0 - BODY_MATCH_B + BODY_MATCH_B * normalized);
        let score = counts
            .iter()
            .map(|(term, count)| {
                let frequency = f64::from(*count);
                let saturation = frequency * (BODY_MATCH_K1 + 1.0) / (frequency + length_factor);
                frequencies.inverse_frequency(term) * saturation
            })
            .sum();
        self.scored.push((score, declaration));
    }

    /// Whether no declaration joined the pool.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scored.is_empty()
    }

    /// The pooled declarations, best score first; equal scores keep the order they
    /// joined in.
    #[must_use]
    pub fn into_ranked(mut self) -> Vec<T> {
        self.scored
            .sort_by(|left, right| right.0.total_cmp(&left.0));
        self.scored
            .into_iter()
            .map(|(_, declaration)| declaration)
            .collect()
    }
}

/// Widens a row count or a byte length into the floating-point domain a score is computed
/// in. Counts and lengths here are bounded by the store's row count and a file's size,
/// far below the range where the conversion loses a unit that could change an order.
#[expect(
    clippy::cast_precision_loss,
    reason = "row counts and byte lengths sit far below 2^52"
)]
const fn as_float(value: u64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{BodyMatchPool, BodyTerms, FileRowFrequencies};
    use crate::query::ParsedQuery;

    fn terms(query: &str) -> BodyTerms {
        BodyTerms::of(&ParsedQuery::parse(query).expect("the query parses"))
    }

    #[test]
    fn test_body_terms_fold_case_and_split_phrases_into_their_tokens() {
        let held = terms("Beacon \"spawn_task now\"");
        assert_eq!(
            held.iter().collect::<Vec<_>>(),
            ["beacon", "now", "spawn", "task"]
        );
        assert!(!held.is_empty());
        assert!(terms("---").is_empty());
    }

    #[test]
    fn test_occurrences_name_each_term_at_its_token_offset() {
        let held = terms("beacon task");
        let text = "fn run() {\r\n    BEACON.task(); beacons(task)\r\n}";
        let found = held.occurrences(text);
        assert_eq!(found, [(16, "beacon"), (23, "task"), (39, "task")]);
        for (offset, term) in found {
            assert_eq!(text[offset..offset + term.len()].to_lowercase(), term);
        }
        assert!(held.occurrences("no match here").is_empty());
    }

    #[test]
    fn test_a_rare_term_outscores_a_common_one_at_equal_counts() {
        let frequencies =
            FileRowFrequencies::new(100, [("rare".to_owned(), 2), ("common".to_owned(), 90)]);
        assert_eq!(frequencies.rows(), 100);
        assert_eq!(frequencies.rows_holding("absent"), 0);
        let mut pool = BodyMatchPool::default();
        pool.add(
            &frequencies,
            "common",
            &BTreeMap::from([("common", 1)]),
            500,
        );
        pool.add(&frequencies, "rare", &BTreeMap::from([("rare", 1)]), 500);
        assert_eq!(pool.into_ranked(), ["rare", "common"]);
    }

    #[test]
    fn test_a_shorter_declaration_outscores_a_longer_one_holding_the_same_terms() {
        let frequencies = FileRowFrequencies::new(100, [("beacon".to_owned(), 3)]);
        let counts = BTreeMap::from([("beacon", 2)]);
        let mut pool = BodyMatchPool::default();
        pool.add(&frequencies, "long", &counts, 40_000);
        pool.add(&frequencies, "short", &counts, 200);
        pool.add(&frequencies, "also_short", &counts, 200);
        assert_eq!(pool.into_ranked(), ["short", "also_short", "long"]);
    }

    #[test]
    fn test_the_score_follows_bm25_with_the_evaluated_constants() {
        let frequencies = FileRowFrequencies::new(10, [("beacon".to_owned(), 1)]);
        let mut pool = BodyMatchPool::default();
        pool.add(&frequencies, (), &BTreeMap::from([("beacon", 3)]), 2_000);
        let (score, ()) = pool.scored[0];
        let inverse = ((10.0_f64 - 1.0 + 0.5) / (1.0 + 0.5)).ln();
        let expected = inverse * 3.0 * 1.9 / (3.0 + 0.9);
        assert!((score - expected).abs() < 1e-12, "{score} != {expected}");
    }

    #[test]
    fn test_a_term_every_row_holds_keeps_the_floor_score() {
        let frequencies = FileRowFrequencies::new(4, [("everywhere".to_owned(), 4)]);
        let mut pool = BodyMatchPool::default();
        assert!(pool.is_empty());
        pool.add(&frequencies, (), &BTreeMap::from([("everywhere", 1)]), 10);
        assert!(!pool.is_empty());
        assert!(pool.scored[0].0 > 0.0, "the floor keeps the score positive");
    }
}
