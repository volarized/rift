//! Weighted reciprocal rank fusion, and the two per-file aggregations it feeds.
//!
//! The lexical and the semantic tier score in different units: one is a BM25
//! rank, the other a cosine. Neither is comparable to the other, and neither
//! becomes comparable by rescaling. What both tiers agree on is the position
//! they put a candidate in, so fusion reads positions alone:
//!
//! ```text
//! score(c) = sum over rankings i of  weight_i / (fusion_k + rank_i(c))
//! ```
//!
//! `rank_i(c)` is the 1-based position of `c` in ranking `i`, and a ranking
//! that never returned `c` contributes nothing. `fusion_k` is the operator's
//! constant: it flattens the head of each ranking, so a first place is worth
//! more than a second without being worth more than every other ranking's
//! opinion combined.
//!
//! Everything here is sans-I/O: the rankings arrive as values two tiers already
//! produced.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use rift_core::ProjectPath;

use crate::error::{SearchError, SearchFault, SearchViolation};
use crate::similarity::SemanticMatch;

/// One ranking handed to fusion: the share it carries of a fused score, and
/// the identities it ranked, best first.
#[derive(Clone, Copy, Debug)]
pub struct Ranking<'a> {
    weight: f64,
    order: &'a [&'a str],
}

impl<'a> Ranking<'a> {
    /// Names one ranking's weight and the identities it returned.
    #[must_use]
    pub const fn new(weight: f64, order: &'a [&'a str]) -> Self {
        Self { weight, order }
    }

    /// The share of a fused score this ranking carries.
    #[must_use]
    pub const fn weight(self) -> f64 {
        self.weight
    }

    /// The identities this ranking returned, best first.
    #[must_use]
    pub const fn order(self) -> &'a [&'a str] {
        self.order
    }
}

/// One fused result.
#[derive(Clone, Debug, PartialEq)]
pub struct FusedRank {
    identity: String,
    score: f64,
}

impl FusedRank {
    /// The identity this score belongs to.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// The fused score, `1.0` for a candidate every ranking put first.
    #[must_use]
    pub const fn score(&self) -> f64 {
        self.score
    }
}

/// Fuses `rankings` by weighted reciprocal rank.
///
/// The returned score is normalized against the score a candidate ranked first
/// in every ranking would reach, `sum_i weight_i / (fusion_k + 1)`, so a
/// candidate at the top of both tiers scores exactly `1.0` and two queries'
/// scores mean the same thing. Dividing by a positive constant leaves the order
/// the raw formula produces unchanged.
///
/// A duplicate identity inside one ranking keeps its best position: the first
/// occurrence is the rank the formula reads, and a later repeat contributes
/// nothing.
///
/// No ranking at all, and rankings that all returned nothing, fuse to nothing
/// rather than refusing. The order is descending score, ties broken by identity
/// ascending, truncated to `keep_max`. Each loop runs once over a slice whose
/// length is already the caller's own bound.
///
/// # Errors
///
/// Returns `ranking_weights_invalid` when a weight is negative or is not a
/// finite number, or when the weights sum to zero or less: no share of the
/// score is then left to distribute, and the subject states the sum. Returns
/// the same violation when `fusion_k` is zero, which divides but applies no
/// bound at all; the accepted range is 1 to 1000.
pub fn fuse(
    rankings: &[Ranking<'_>],
    fusion_k: u64,
    keep_max: usize,
) -> Result<Vec<FusedRank>, SearchError> {
    if rankings.is_empty() {
        return Ok(Vec::new());
    }
    let weights_sum = weights_sum(rankings);
    if let Some(subject) = weights_refusal(rankings, weights_sum) {
        return Err(weights_invalid(subject));
    }
    if fusion_k == 0 {
        return Err(fusion_constant_invalid());
    }
    let mut scores: BTreeMap<&str, f64> = BTreeMap::new();
    for ranking in rankings {
        accumulate(&mut scores, *ranking, fusion_k);
    }
    let mut fused: Vec<FusedRank> = scores
        .into_iter()
        .map(|(identity, score)| FusedRank {
            identity: identity.to_owned(),
            score: score / weights_sum,
        })
        .collect();
    fused.sort_unstable_by(strongest_first);
    fused.truncate(keep_max);
    Ok(fused)
}

/// The weights as the normalization reads them, in the order given.
///
/// Fusion adds each ranking's share in this same order, so a candidate every
/// ranking put first sums to exactly this value and normalizes to exactly
/// `1.0`.
fn weights_sum(rankings: &[Ranking<'_>]) -> f64 {
    rankings.iter().copied().map(Ranking::weight).sum()
}

/// What is wrong with the weights, as the subject a refusal carries.
fn weights_refusal(rankings: &[Ranking<'_>], weights_sum: f64) -> Option<String> {
    let unusable = rankings
        .iter()
        .any(|ranking| ranking.weight() < 0.0 || !ranking.weight().is_finite());
    if unusable || weights_sum <= 0.0 {
        return Some(format!(
            "weights sum to {weights_sum}, expected each weight finite and not negative, summing above zero"
        ));
    }
    None
}

/// One refusal over the shares a fusion call was handed.
fn weights_invalid(subject: String) -> SearchError {
    SearchError::new(SearchFault::new(SearchViolation::RankingWeightsInvalid).about(subject))
}

/// Refuses a rank constant of zero, which is the operator's bound not being
/// applied rather than a constant that flattens nothing.
fn fusion_constant_invalid() -> SearchError {
    SearchError::new(
        SearchFault::new(SearchViolation::FusionConstantInvalid)
            .about("fusion_k 0, expected 1 to 1000"),
    )
}

/// Adds one ranking's share to every identity it returned.
///
/// The loop runs once over the ranking's own slice. `seen` holds the identities
/// this ranking already paid for, which is what keeps a duplicate at its first
/// position.
fn accumulate<'a>(scores: &mut BTreeMap<&'a str, f64>, ranking: Ranking<'a>, fusion_k: u64) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (position, &identity) in ranking.order().iter().enumerate() {
        if seen.insert(identity) {
            *scores.entry(identity).or_insert(0.0) +=
                contribution(ranking.weight(), fusion_k, position);
        }
    }
}

/// One ranking's share for a candidate at `position`, relative to what a first
/// place carries.
///
/// Scaling by the first-place divisor here, rather than dividing the finished
/// score by `weights_sum / (fusion_k + 1)`, keeps a first place worth exactly
/// its whole weight. The two are the same quantity; only this one reaches
/// exactly `1.0` instead of one unit in the last place away from it.
fn contribution(weight: f64, fusion_k: u64, position: usize) -> f64 {
    weight * (divisor(fusion_k, 0) / divisor(fusion_k, position))
}

/// The formula's divisor: the operator's constant plus the 1-based rank.
fn divisor(fusion_k: u64, position: usize) -> f64 {
    let rank = u64::try_from(position)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    as_f64(fusion_k) + as_f64(rank)
}

/// One count as the formula's arithmetic takes it.
///
/// A count above `u32::MAX` saturates. No ranking a search returns reaches four
/// billion entries, and a term that deep is already below what the sum resolves.
fn as_f64(count: u64) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Descending score, then identity ascending.
fn strongest_first(one: &FusedRank, other: &FusedRank) -> Ordering {
    other
        .score
        .total_cmp(&one.score)
        .then_with(|| one.identity.cmp(&other.identity))
}

/// One semantic match together with the file whose declaration produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclarationMatch {
    file: ProjectPath,
    matched: SemanticMatch,
}

impl DeclarationMatch {
    /// Pairs one semantic match with the file its declaration lives in.
    #[must_use]
    pub const fn new(file: ProjectPath, matched: SemanticMatch) -> Self {
        Self { file, matched }
    }

    /// The file the matched declaration lives in.
    #[must_use]
    pub const fn file(&self) -> &ProjectPath {
        &self.file
    }

    /// The match that declaration reached.
    #[must_use]
    pub const fn matched(&self) -> &SemanticMatch {
        &self.matched
    }
}

/// Collapses `matches` to one entry per file, keeping the best similarity any
/// declaration in that file reached.
///
/// A file's semantic rank is the best its declarations reached: a query that
/// matched one function has matched the file that holds it, and the file's
/// other declarations say nothing about that.
///
/// Files come back in descending best similarity, ties broken by path
/// ascending. The loop runs once over a slice whose length is already the
/// caller's own bound.
#[must_use]
pub fn best_per_file(matches: &[DeclarationMatch]) -> Vec<DeclarationMatch> {
    let mut best: BTreeMap<&ProjectPath, &DeclarationMatch> = BTreeMap::new();
    for entry in matches {
        keep_better(&mut best, entry);
    }
    let mut kept: Vec<DeclarationMatch> = best.into_values().cloned().collect();
    kept.sort_unstable_by(strongest_file_first);
    kept
}

/// Keeps `entry` when its file holds nothing better yet.
///
/// A tie keeps the entry already held, so one input order produces one answer.
fn keep_better<'a>(
    best: &mut BTreeMap<&'a ProjectPath, &'a DeclarationMatch>,
    entry: &'a DeclarationMatch,
) {
    let better = match best.get(entry.file()) {
        Some(held) => entry.matched().similarity() > held.matched().similarity(),
        None => true,
    };
    if better {
        best.insert(entry.file(), entry);
    }
}

/// Keeps at most `per_file_max` matches from any one file, in the order given.
///
/// Without the cap, one large file's declarations crowd every other file out of
/// a bounded candidate list, and the tier that follows never sees the file that
/// held the second-best answer.
///
/// The input order is preserved exactly: only the overflow past `per_file_max`
/// is dropped, and a cap of zero keeps nothing. The loop runs once over a slice
/// whose length is already the caller's own bound.
#[must_use]
pub fn spread_per_file(matches: &[DeclarationMatch], per_file_max: usize) -> Vec<DeclarationMatch> {
    if per_file_max == 0 {
        return Vec::new();
    }
    let mut taken: BTreeMap<&ProjectPath, usize> = BTreeMap::new();
    let mut kept: Vec<DeclarationMatch> = Vec::with_capacity(matches.len());
    for entry in matches {
        let count = taken.entry(entry.file()).or_insert(0);
        if *count < per_file_max {
            *count += 1;
            kept.push(entry.clone());
        }
    }
    kept
}

/// Descending similarity, then path ascending.
fn strongest_file_first(one: &DeclarationMatch, other: &DeclarationMatch) -> Ordering {
    other
        .matched()
        .similarity()
        .total_cmp(&one.matched().similarity())
        .then_with(|| one.file().cmp(other.file()))
}

#[cfg(test)]
mod tests {
    use super::{
        DeclarationMatch, FusedRank, Ranking, as_f64, contribution, divisor, keep_better,
        strongest_file_first, strongest_first, weights_refusal, weights_sum,
    };
    use crate::similarity::SemanticMatch;
    use rift_core::ProjectPath;
    use std::cmp::Ordering;
    use std::collections::BTreeMap;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Whether two scores agree to double-precision accuracy. Fusion divides
    /// twice, so the two sides differ by the rounding of those divisions.
    fn is_score(computed: f64, expected: f64) -> bool {
        (computed - expected).abs() < 1e-12
    }

    fn declaration(
        path: &str,
        digest: &str,
        similarity: f32,
    ) -> Result<DeclarationMatch, Box<dyn std::error::Error>> {
        let file = ProjectPath::new(path.to_owned())?;
        Ok(DeclarationMatch::new(
            file,
            SemanticMatch::new(digest.to_owned(), similarity),
        ))
    }

    #[test]
    fn test_a_first_place_carries_its_whole_weight() {
        assert!(is_score(contribution(0.6, 60, 0), 0.6));
        assert!(is_score(contribution(0.4, 1, 0), 0.4));
        assert!(
            is_score(contribution(0.6, 60, 1), 0.6 * 61.0 / 62.0),
            "a second place is flattened by the operator's constant"
        );
    }

    #[test]
    fn test_the_divisor_is_the_constant_plus_the_rank() {
        assert!(is_score(divisor(60, 0), 61.0));
        assert!(is_score(divisor(1, 3), 5.0));
        assert!(
            is_score(divisor(1, usize::MAX), 1.0 + f64::from(u32::MAX)),
            "a position no slice can hold saturates rather than wrapping"
        );
        assert!(is_score(as_f64(0), 0.0));
        assert!(is_score(as_f64(u64::MAX), f64::from(u32::MAX)));
    }

    #[test]
    fn test_weights_are_summed_in_the_order_they_were_given() {
        let one = ["a"];
        let rankings = [Ranking::new(0.7, &one), Ranking::new(0.3, &one)];
        assert!(is_score(weights_sum(&rankings), 1.0));
        assert_eq!(weights_refusal(&rankings, weights_sum(&rankings)), None);
    }

    #[test]
    fn test_weights_that_cannot_carry_a_score_state_their_sum() {
        let one = ["a"];
        let negative = [Ranking::new(-1.0, &one), Ranking::new(2.0, &one)];
        let refusal = weights_refusal(&negative, weights_sum(&negative)).unwrap_or_default();
        assert!(refusal.contains("weights sum to 1"), "{refusal}");
        let zero = [Ranking::new(0.0, &one)];
        assert!(weights_refusal(&zero, weights_sum(&zero)).is_some());
        let infinite = [Ranking::new(f64::INFINITY, &one)];
        assert!(weights_refusal(&infinite, weights_sum(&infinite)).is_some());
    }

    #[test]
    fn test_the_fused_order_is_score_then_identity() {
        let strong = FusedRank {
            identity: "bbb".to_owned(),
            score: 1.0,
        };
        let weak = FusedRank {
            identity: "aaa".to_owned(),
            score: 0.5,
        };
        assert_eq!(strongest_first(&strong, &weak), Ordering::Less);
        let tied = FusedRank {
            identity: "ccc".to_owned(),
            score: 1.0,
        };
        assert_eq!(strongest_first(&strong, &tied), Ordering::Less);
        assert_eq!(strongest_first(&tied, &strong), Ordering::Greater);
    }

    #[test]
    fn test_the_file_order_is_similarity_then_path() -> TestResult {
        let strong = declaration("src/b.rs", "aaa", 0.9)?;
        let weak = declaration("src/a.rs", "bbb", 0.1)?;
        assert_eq!(strongest_file_first(&strong, &weak), Ordering::Less);
        let tied = declaration("src/c.rs", "ccc", 0.9)?;
        assert_eq!(strongest_file_first(&strong, &tied), Ordering::Less);
        assert_eq!(strongest_file_first(&tied, &strong), Ordering::Greater);
        Ok(())
    }

    #[test]
    fn test_a_file_keeps_the_first_of_two_equally_good_declarations() -> TestResult {
        let first = declaration("src/a.rs", "aaa", 0.5)?;
        let second = declaration("src/a.rs", "bbb", 0.5)?;
        let mut best: BTreeMap<&ProjectPath, &DeclarationMatch> = BTreeMap::new();
        keep_better(&mut best, &first);
        keep_better(&mut best, &second);
        assert_eq!(
            best.get(first.file()).map(|held| held.matched().digest()),
            Some("aaa")
        );
        Ok(())
    }
}
