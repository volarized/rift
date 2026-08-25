//! Fusion over rankings built in the test, so no suite opens an index.

use rift_core::ProjectPath;
use rift_search::{
    DeclarationMatch, FusedRank, Ranking, SearchViolation, SemanticMatch, best_per_file, fuse,
    spread_per_file,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// The constant the operator's default applies, and the two ends of the range
/// the configuration accepts.
const FUSION_K: u64 = 60;
const FUSION_K_MIN: u64 = 1;
const FUSION_K_MAX: u64 = 1_000;

/// Whether two scores agree to double-precision accuracy. Fusion divides twice,
/// so the two sides differ by the rounding of those divisions.
fn is_score(computed: f64, expected: f64) -> bool {
    (computed - expected).abs() < 1e-12
}

/// Whether a score is the exact value, which the normalization guarantees for a
/// candidate every ranking put first. `total_cmp` compares without the equality
/// an inexact score would need.
fn is_exactly(computed: f64, expected: f64) -> bool {
    computed.total_cmp(&expected).is_eq()
}

fn identities(fused: &[FusedRank]) -> Vec<&str> {
    fused.iter().map(FusedRank::identity).collect()
}

fn declaration(
    path: &str,
    digest: &str,
    similarity: f32,
) -> Result<DeclarationMatch, Box<dyn std::error::Error + Send + Sync>> {
    let file = ProjectPath::new(path.to_owned())?;
    Ok(DeclarationMatch::new(
        file,
        SemanticMatch::new(digest.to_owned(), similarity),
    ))
}

fn paths(matches: &[DeclarationMatch]) -> Vec<&str> {
    matches.iter().map(|entry| entry.file().as_str()).collect()
}

fn digests(matches: &[DeclarationMatch]) -> Vec<&str> {
    matches
        .iter()
        .map(|entry| entry.matched().digest())
        .collect()
}

#[test]
fn the_fused_score_is_each_rankings_weight_over_the_rank_it_gave() -> TestResult {
    let lexical = ["a", "b", "c"];
    let semantic = ["c", "a"];
    let fused = fuse(
        &[Ranking::new(0.6, &lexical), Ranking::new(0.4, &semantic)],
        FUSION_K,
        8,
    )?;
    assert_eq!(identities(&fused), ["a", "c", "b"]);
    let top = 1.0 + f64::from(u32::try_from(FUSION_K)?);
    let expected_a = 0.6 + 0.4 * (top / (top + 1.0));
    let expected_c = 0.6 * (top / (top + 2.0)) + 0.4;
    let expected_b = 0.6 * (top / (top + 1.0));
    assert!(is_score(fused[0].score(), expected_a), "{:?}", fused[0]);
    assert!(is_score(fused[1].score(), expected_c), "{:?}", fused[1]);
    assert!(is_score(fused[2].score(), expected_b), "{:?}", fused[2]);
    Ok(())
}

#[test]
fn a_candidate_every_ranking_put_first_scores_exactly_one() -> TestResult {
    let lexical = ["shared", "other"];
    let semantic = ["shared"];
    let fused = fuse(
        &[Ranking::new(0.6, &lexical), Ranking::new(0.4, &semantic)],
        FUSION_K,
        8,
    )?;
    assert!(
        is_exactly(fused[0].score(), 1.0),
        "the top of both rankings is the score the normalization divides by: {:?}",
        fused[0]
    );
    assert!(
        fused[1].score() < 1.0,
        "everything else scores below it: {:?}",
        fused[1]
    );
    Ok(())
}

#[test]
fn a_candidate_one_ranking_alone_returned_carries_that_rankings_share() -> TestResult {
    let lexical = ["only"];
    let semantic = ["elsewhere"];
    let fused = fuse(
        &[Ranking::new(0.75, &lexical), Ranking::new(0.25, &semantic)],
        FUSION_K,
        8,
    )?;
    assert_eq!(identities(&fused), ["only", "elsewhere"]);
    assert!(is_score(fused[0].score(), 0.75), "{:?}", fused[0]);
    assert!(is_score(fused[1].score(), 0.25), "{:?}", fused[1]);
    Ok(())
}

#[test]
fn weights_that_sum_to_zero_are_refused_with_the_sum() {
    let order = ["a"];
    let error = fuse(
        &[Ranking::new(0.0, &order), Ranking::new(0.0, &order)],
        FUSION_K,
        8,
    )
    .expect_err("no share of the score is left to distribute");
    assert_eq!(
        error.fault().violation(),
        SearchViolation::RankingWeightsInvalid
    );
    let rendered = error.to_string();
    assert!(rendered.contains("ranking_weights_invalid"), "{rendered}");
    assert!(rendered.contains("weights sum to 0"), "{rendered}");
}

#[test]
fn a_negative_weight_is_refused_even_when_the_sum_stands() {
    let order = ["a"];
    let error = fuse(
        &[Ranking::new(-1.0, &order), Ranking::new(2.0, &order)],
        FUSION_K,
        8,
    )
    .expect_err("a ranking cannot carry less than nothing");
    assert_eq!(
        error.fault().violation(),
        SearchViolation::RankingWeightsInvalid
    );
    assert!(
        error.to_string().contains("not negative"),
        "the refusal states what a weight must be: {error}"
    );
}

#[test]
fn a_weight_that_is_not_a_finite_number_is_refused() {
    let order = ["a"];
    for weight in [f64::INFINITY, f64::NAN] {
        let error = fuse(&[Ranking::new(weight, &order)], FUSION_K, 8)
            .expect_err("a share that is not a number distributes nothing");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::RankingWeightsInvalid
        );
    }
}

#[test]
fn a_fusion_constant_of_zero_is_refused_as_a_bound_never_applied() {
    let order = ["a"];
    let error = fuse(&[Ranking::new(1.0, &order)], 0, 8)
        .expect_err("a zero constant is the operator's bound not being applied");
    assert_eq!(
        error.fault().violation(),
        SearchViolation::FusionConstantInvalid
    );
    assert!(
        error.to_string().contains("fusion_k 0, expected 1 to 1000"),
        "the refusal names the accepted range: {error}"
    );
}

#[test]
fn a_fusion_constant_at_either_end_of_its_range_fuses() -> TestResult {
    let lexical = ["a", "b"];
    let semantic = ["b", "a"];
    for fusion_k in [FUSION_K_MIN, FUSION_K_MAX] {
        let fused = fuse(
            &[Ranking::new(0.5, &lexical), Ranking::new(0.5, &semantic)],
            fusion_k,
            8,
        )?;
        assert_eq!(identities(&fused), ["a", "b"]);
        assert!(
            is_score(fused[0].score(), fused[1].score()),
            "one symmetric pair scores alike at fusion_k {fusion_k}"
        );
    }
    let flat = fuse(&[Ranking::new(1.0, &lexical)], FUSION_K_MAX, 8)?;
    let steep = fuse(&[Ranking::new(1.0, &lexical)], FUSION_K_MIN, 8)?;
    assert!(
        flat[1].score() > steep[1].score(),
        "the larger constant flattens the head: {flat:?} against {steep:?}"
    );
    Ok(())
}

#[test]
fn a_duplicate_identity_keeps_its_first_position() -> TestResult {
    let repeated = ["a", "b", "a"];
    let once = ["a", "b"];
    let fused = fuse(&[Ranking::new(1.0, &repeated)], FUSION_K, 8)?;
    let single = fuse(&[Ranking::new(1.0, &once)], FUSION_K, 8)?;
    assert_eq!(fused.len(), 2, "one identity holds one place");
    assert_eq!(fused, single, "a later repeat contributes nothing");
    Ok(())
}

#[test]
fn no_ranking_at_all_fuses_to_nothing() -> TestResult {
    assert!(fuse(&[], FUSION_K, 8)?.is_empty());
    Ok(())
}

#[test]
fn rankings_that_returned_nothing_fuse_to_nothing() -> TestResult {
    let empty: [&str; 0] = [];
    let fused = fuse(
        &[Ranking::new(0.5, &empty), Ranking::new(0.5, &empty)],
        FUSION_K,
        8,
    )?;
    assert!(fused.is_empty(), "two empty rankings agree on nothing");
    Ok(())
}

#[test]
fn the_result_is_truncated_to_what_the_caller_keeps() -> TestResult {
    let order = ["a", "b", "c"];
    assert!(fuse(&[Ranking::new(1.0, &order)], FUSION_K, 0)?.is_empty());
    let one = fuse(&[Ranking::new(1.0, &order)], FUSION_K, 1)?;
    assert_eq!(identities(&one), ["a"]);
    let every = fuse(&[Ranking::new(1.0, &order)], FUSION_K, 99)?;
    assert_eq!(identities(&every), ["a", "b", "c"]);
    Ok(())
}

#[test]
fn two_candidates_of_one_score_are_ordered_by_identity() -> TestResult {
    let lexical = ["zebra", "alpha"];
    let semantic = ["alpha", "zebra"];
    let fused = fuse(
        &[Ranking::new(0.5, &lexical), Ranking::new(0.5, &semantic)],
        FUSION_K,
        8,
    )?;
    assert_eq!(identities(&fused), ["alpha", "zebra"]);
    assert!(is_score(fused[0].score(), fused[1].score()));
    Ok(())
}

#[test]
fn more_than_two_rankings_each_carry_their_share() -> TestResult {
    let one = ["a"];
    let two = ["a"];
    let three = ["b"];
    let fused = fuse(
        &[
            Ranking::new(0.5, &one),
            Ranking::new(0.3, &two),
            Ranking::new(0.2, &three),
        ],
        FUSION_K,
        8,
    )?;
    assert_eq!(identities(&fused), ["a", "b"]);
    assert!(
        is_score(fused[0].score(), 0.8),
        "two of three rankings put it first: {:?}",
        fused[0]
    );
    assert!(is_score(fused[1].score(), 0.2), "{:?}", fused[1]);
    Ok(())
}

#[test]
fn the_debug_render_names_a_ranking_and_a_fused_result() -> TestResult {
    let order = ["a"];
    let ranking = Ranking::new(0.5, &order);
    let rendered = format!("{ranking:?}");
    assert!(rendered.starts_with("Ranking"), "{rendered}");
    assert!(rendered.contains("weight: 0.5"), "{rendered}");
    assert!(rendered.contains("\"a\""), "{rendered}");
    assert_eq!(ranking.order(), ["a"]);
    let fused = fuse(&[ranking], FUSION_K, 1)?;
    let rendered = format!("{:?}", fused[0]);
    assert!(rendered.starts_with("FusedRank"), "{rendered}");
    assert!(rendered.contains("identity: \"a\""), "{rendered}");
    assert!(rendered.contains("score: 1.0"), "{rendered}");
    Ok(())
}

#[test]
fn a_file_ranks_by_the_best_its_declarations_reached() -> TestResult {
    let matches = [
        declaration("src/index.rs", "aaa", 0.2)?,
        declaration("src/index.rs", "bbb", 0.9)?,
        declaration("src/index.rs", "ccc", 0.5)?,
        declaration("src/search.rs", "ddd", 0.7)?,
    ];
    let best = best_per_file(&matches);
    assert_eq!(paths(&best), ["src/index.rs", "src/search.rs"]);
    assert_eq!(
        digests(&best),
        ["bbb", "ddd"],
        "the file carries the declaration that reached its rank"
    );
    Ok(())
}

#[test]
fn files_of_one_similarity_are_ordered_by_path() -> TestResult {
    let matches = [
        declaration("src/zebra.rs", "aaa", 0.5)?,
        declaration("src/alpha.rs", "bbb", 0.5)?,
    ];
    assert_eq!(
        paths(&best_per_file(&matches)),
        ["src/alpha.rs", "src/zebra.rs"]
    );
    Ok(())
}

#[test]
fn no_declarations_collapse_to_no_files() {
    assert!(best_per_file(&[]).is_empty());
}

#[test]
fn one_declaration_is_its_own_files_rank() -> TestResult {
    let matches = [declaration("src/only.rs", "aaa", 0.4)?];
    let best = best_per_file(&matches);
    assert_eq!(best, matches.to_vec());
    Ok(())
}

#[test]
fn a_per_file_cap_of_zero_keeps_nothing() -> TestResult {
    let matches = [declaration("src/index.rs", "aaa", 0.9)?];
    assert!(spread_per_file(&matches, 0).is_empty());
    assert!(spread_per_file(&[], 4).is_empty());
    Ok(())
}

#[test]
fn a_per_file_cap_drops_only_the_overflow_and_keeps_the_order() -> TestResult {
    let matches = [
        declaration("src/index.rs", "aaa", 0.9)?,
        declaration("src/search.rs", "bbb", 0.8)?,
        declaration("src/index.rs", "ccc", 0.7)?,
        declaration("src/index.rs", "ddd", 0.6)?,
        declaration("src/search.rs", "eee", 0.5)?,
    ];
    assert_eq!(
        digests(&spread_per_file(&matches, 1)),
        ["aaa", "bbb"],
        "one large file cannot crowd the other out"
    );
    assert_eq!(
        digests(&spread_per_file(&matches, 2)),
        ["aaa", "bbb", "ccc", "eee"],
        "the input order is preserved, and only the overflow is dropped"
    );
    Ok(())
}

#[test]
fn a_per_file_cap_above_the_count_keeps_every_match() -> TestResult {
    let matches = [
        declaration("src/index.rs", "aaa", 0.9)?,
        declaration("src/search.rs", "bbb", 0.8)?,
        declaration("src/index.rs", "ccc", 0.7)?,
    ];
    assert_eq!(spread_per_file(&matches, 9), matches.to_vec());
    Ok(())
}

#[test]
fn the_debug_render_names_the_file_and_the_match_it_carries() -> TestResult {
    let matched = declaration("src/index.rs", "aaa", 0.25)?;
    let rendered = format!("{matched:?}");
    assert!(rendered.starts_with("DeclarationMatch"), "{rendered}");
    assert!(rendered.contains("src/index.rs"), "{rendered}");
    assert!(rendered.contains("SemanticMatch"), "{rendered}");
    assert_eq!(matched.file().as_str(), "src/index.rs");
    assert_eq!(matched.matched().digest(), "aaa");
    Ok(())
}
