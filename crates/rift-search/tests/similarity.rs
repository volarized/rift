//! Cosine ranking over a corpus built in the test, so no suite loads a model.

use rift_index::StoredVector;
use rift_search::{SearchViolation, SemanticMatch, nearest};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// The width every fixture vector carries.
const WIDTH: usize = 3;

/// The direction every fixture is scored against.
const QUERY: [f32; WIDTH] = [1.0, 0.0, 0.0];

/// Whether two similarities agree to single-precision accuracy. Cosine divides,
/// so the two sides differ by the rounding of that division.
fn is_similarity(computed: f32, expected: f32) -> bool {
    (computed - expected).abs() < 1e-6
}

fn stored(digest: &str, values: [f32; WIDTH]) -> StoredVector {
    StoredVector::new(digest.to_owned(), values.to_vec())
}

fn digests(matches: &[SemanticMatch]) -> Vec<&str> {
    matches.iter().map(SemanticMatch::digest).collect()
}

/// One corpus holding the query's own direction, one at 45 degrees to it, one
/// orthogonal, and the query's opposite. The strongest rows sit last, so a scan
/// that kept the corpus order would answer with the weakest.
fn corpus() -> Vec<StoredVector> {
    vec![
        stored("opposed", [-1.0, 0.0, 0.0]),
        stored("orthogonal", [0.0, 1.0, 0.0]),
        stored("slanted", [1.0, 1.0, 0.0]),
        stored("identical", [1.0, 0.0, 0.0]),
    ]
}

#[test]
fn a_query_with_no_values_is_refused_before_any_vector_is_scanned() {
    let error = nearest(&[], &corpus(), 4).expect_err("a query of no values ranks nothing");
    assert_eq!(
        error.fault().violation(),
        SearchViolation::VectorWidthMismatch
    );
    assert!(
        error.to_string().contains("query width 0, stored width 3"),
        "the refusal names both widths: {error}"
    );
}

#[test]
fn a_corpus_row_of_another_width_is_refused_and_the_message_names_both() {
    let mut rows = corpus();
    rows.push(StoredVector::new("narrow".to_owned(), vec![1.0, 0.0]));
    let error = nearest(&QUERY, &rows, 4).expect_err("a row of another width is not an answer");
    assert_eq!(
        error.fault().violation(),
        SearchViolation::VectorWidthMismatch
    );
    let rendered = error.to_string();
    assert!(rendered.contains("vector_width_mismatch"), "{rendered}");
    assert!(
        rendered.contains("query width 3, stored width 2"),
        "{rendered}"
    );
}

#[test]
fn an_empty_corpus_ranks_nothing_and_refuses_nothing() -> TestResult {
    assert!(nearest(&QUERY, &[], 8)?.is_empty());
    Ok(())
}

#[test]
fn keeping_nothing_returns_nothing() -> TestResult {
    assert!(nearest(&QUERY, &corpus(), 0)?.is_empty());
    Ok(())
}

#[test]
fn keeping_one_returns_the_single_strongest_row() -> TestResult {
    let ranked = nearest(&QUERY, &corpus(), 1)?;
    assert_eq!(digests(&ranked), ["identical"]);
    assert!(is_similarity(ranked[0].similarity(), 1.0));
    Ok(())
}

#[test]
fn keeping_exactly_the_corpus_size_returns_every_row_in_order() -> TestResult {
    let rows = corpus();
    let ranked = nearest(&QUERY, &rows, rows.len())?;
    assert_eq!(
        digests(&ranked),
        ["identical", "slanted", "orthogonal", "opposed"]
    );
    Ok(())
}

#[test]
fn keeping_more_than_the_corpus_holds_returns_every_row() -> TestResult {
    let rows = corpus();
    let ranked = nearest(&QUERY, &rows, rows.len() * 4)?;
    assert_eq!(ranked.len(), rows.len());
    assert_eq!(digests(&ranked)[0], "identical");
    Ok(())
}

#[test]
fn the_query_itself_ranks_first_and_its_opposite_ranks_last() -> TestResult {
    let ranked = nearest(&QUERY, &corpus(), 4)?;
    assert!(is_similarity(ranked[0].similarity(), 1.0));
    assert_eq!(ranked[0].digest(), "identical");
    let last = ranked.len() - 1;
    assert!(is_similarity(ranked[last].similarity(), -1.0));
    assert_eq!(ranked[last].digest(), "opposed");
    assert!(
        is_similarity(ranked[1].similarity(), 0.5_f32.sqrt()),
        "a vector at 45 degrees scores the cosine of 45 degrees"
    );
    Ok(())
}

#[test]
fn a_row_without_direction_scores_zero_rather_than_a_value_that_is_not_a_number() -> TestResult {
    let rows = vec![
        stored("zero", [0.0, 0.0, 0.0]),
        stored("identical", [1.0, 0.0, 0.0]),
    ];
    let ranked = nearest(&QUERY, &rows, 2)?;
    assert_eq!(digests(&ranked), ["identical", "zero"]);
    let scored = ranked[1].similarity();
    assert!(scored.is_finite(), "a zero magnitude must not divide");
    assert!(is_similarity(scored, 0.0));
    Ok(())
}

#[test]
fn a_stored_row_of_another_length_still_scores_as_a_cosine() -> TestResult {
    let rows = vec![
        stored("unit", [1.0, 0.0, 0.0]),
        stored("long", [8.0, 0.0, 0.0]),
    ];
    let ranked = nearest(&QUERY, &rows, 2)?;
    assert!(
        is_similarity(ranked[0].similarity(), 1.0) && is_similarity(ranked[1].similarity(), 1.0),
        "the magnitudes divide out, so a row that is not unit length ranks with one that is"
    );
    assert_eq!(
        digests(&ranked),
        ["long", "unit"],
        "one similarity is ordered by digest"
    );
    Ok(())
}

#[test]
fn rows_of_one_similarity_are_ordered_by_digest() -> TestResult {
    let rows = vec![
        stored("ccc", [1.0, 0.0, 0.0]),
        stored("aaa", [1.0, 0.0, 0.0]),
        stored("bbb", [1.0, 0.0, 0.0]),
    ];
    let ranked = nearest(&QUERY, &rows, 3)?;
    assert_eq!(digests(&ranked), ["aaa", "bbb", "ccc"]);
    let head = nearest(&QUERY, &rows, 2)?;
    assert_eq!(
        digests(&head),
        ["aaa", "bbb"],
        "the head of a tie is the same head every run"
    );
    Ok(())
}

#[test]
fn the_head_is_the_strongest_rows_wherever_they_sit_in_the_corpus() -> TestResult {
    let mut rows: Vec<StoredVector> = (0..12)
        .map(|index| stored(&format!("weak{index:02}"), [0.0, 1.0, 0.0]))
        .collect();
    rows.push(stored("best", [1.0, 0.0, 0.0]));
    rows.push(stored("second", [1.0, 1.0, 0.0]));
    let ranked = nearest(&QUERY, &rows, 2)?;
    assert_eq!(
        digests(&ranked),
        ["best", "second"],
        "the head is the top of the corpus, not its first rows"
    );
    Ok(())
}

#[test]
fn the_debug_render_names_the_digest_and_the_similarity() -> TestResult {
    let ranked = nearest(&QUERY, &corpus(), 1)?;
    let rendered = format!("{:?}", ranked[0]);
    assert!(rendered.starts_with("SemanticMatch"), "{rendered}");
    assert!(rendered.contains("digest: \"identical\""), "{rendered}");
    assert!(rendered.contains("similarity: 1.0"), "{rendered}");
    Ok(())
}
