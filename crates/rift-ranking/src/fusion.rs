//! Weighted reciprocal rank fusion over ordered document identities.
//!
//! The three inputs score in units nothing converts between: an identifier
//! match class, a BM25 rank, and a cosine. What all three agree on is the
//! position they put a candidate in, so fusion reads positions alone:
//!
//! ```text
//! score(c) = sum over answering inputs i of  weight_i / (fusion_k + rank_i(c))
//! ```
//!
//! `rank_i(c)` is the 1-based position of `c` in input `i`, and an input that
//! never returned `c` contributes nothing. `fusion_k` flattens the head of
//! each input, so a first place is worth more than a second without being
//! worth more than every other input's opinion combined.
//!
//! Only inputs that answered take part, and their weights normalize against
//! each other. An unavailable vector ranking therefore leaves the identifier
//! and full-text order exactly as it was, rather than shrinking every score by
//! the share the missing input would have carried.
//!
//! Nothing here holds a project path or resolves a symbol. The read path
//! resolves identities after ranking, which is what lets a package index rank
//! `SourceUnitId` values through this same code.

use std::collections::BTreeMap;

use crate::document::{DocumentIdentity, FieldSet};
use crate::error::{RankingError, RankingFault, RankingViolation};
use crate::query::QueryPhase;

/// `fusion_k` accepted, at least.
pub const FUSION_K_MIN: u64 = 1;
/// `fusion_k` accepted, at most.
pub const FUSION_K_MAX: u64 = 1_000;

/// Which ranking produced an ordered list.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RankingInputKind {
    /// Identifiers the caller's query carried, matched against indexed names.
    Identifier,
    /// `SQLite` FTS5 over the searchable fields.
    Lexical,
    /// Vector similarity over the embedded documents.
    Vector,
}

impl RankingInputKind {
    /// Every ranking input, in declaration order.
    pub const ALL: [Self; 3] = [Self::Identifier, Self::Lexical, Self::Vector];

    /// The spelling an answer and a log field carry.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Identifier => "identifier",
            Self::Lexical => "lexical",
            Self::Vector => "vector",
        }
    }

    /// This input's bit position in a [`RankingInputSet`].
    const fn position(self) -> u8 {
        match self {
            Self::Identifier => 0,
            Self::Lexical => 1,
            Self::Vector => 2,
        }
    }
}

/// A set of ranking inputs: which ones contributed to one candidate.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RankingInputSet(u8);

impl RankingInputSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// The set holding one input.
    #[must_use]
    pub const fn of(kind: RankingInputKind) -> Self {
        Self(1 << kind.position())
    }

    /// Whether this set holds `kind`.
    #[must_use]
    pub const fn holds(self, kind: RankingInputKind) -> bool {
        self.0 & Self::of(kind).0 != 0
    }

    /// Whether this set holds no input at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// This set with `kind` added.
    #[must_use]
    pub const fn with(self, kind: RankingInputKind) -> Self {
        Self(self.0 | Self::of(kind).0)
    }

    /// The union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The inputs in this set, in declaration order.
    pub fn kinds(self) -> impl Iterator<Item = RankingInputKind> {
        RankingInputKind::ALL
            .into_iter()
            .filter(move |kind| self.holds(*kind))
    }
}

impl FromIterator<RankingInputKind> for RankingInputSet {
    fn from_iter<I: IntoIterator<Item = RankingInputKind>>(kinds: I) -> Self {
        kinds.into_iter().fold(Self::EMPTY, Self::with)
    }
}

/// One identity an input ranked, and the fields that placed it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RankedIdentity {
    identity: DocumentIdentity,
    fields: FieldSet,
}

impl RankedIdentity {
    /// Names one ranked identity and the fields that matched.
    #[must_use]
    pub const fn new(identity: DocumentIdentity, fields: FieldSet) -> Self {
        Self { identity, fields }
    }

    /// The ranked identity.
    #[must_use]
    pub const fn identity(&self) -> &DocumentIdentity {
        &self.identity
    }

    /// The fields that placed this identity.
    #[must_use]
    pub const fn fields(&self) -> FieldSet {
        self.fields
    }
}

/// One input handed to fusion: what produced it, and the identities it
/// ranked, best first.
///
/// The share an input carries lives in [`RankingWeights`] alone. A reader
/// answers with an order; the operator decides what that order is worth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RankingInput {
    kind: RankingInputKind,
    order: Vec<RankedIdentity>,
}

impl RankingInput {
    /// Names one input's origin and ordered answer.
    #[must_use]
    pub const fn new(kind: RankingInputKind, order: Vec<RankedIdentity>) -> Self {
        Self { kind, order }
    }

    /// An input that did not answer, so fusion leaves it out and redistributes
    /// its share across the inputs that did.
    #[must_use]
    pub const fn unanswered(kind: RankingInputKind) -> Self {
        Self {
            kind,
            order: Vec::new(),
        }
    }

    /// What produced this input.
    #[must_use]
    pub const fn kind(&self) -> RankingInputKind {
        self.kind
    }

    /// The identities this input ranked, best first.
    #[must_use]
    pub fn order(&self) -> &[RankedIdentity] {
        &self.order
    }

    /// Whether this input answered at all.
    #[must_use]
    pub fn answered(&self) -> bool {
        !self.order.is_empty()
    }
}

/// The three configured shares and the rank constant they fuse under.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RankingWeights {
    identifier: f64,
    lexical: f64,
    vector: f64,
    fusion_k: u64,
}

impl RankingWeights {
    /// The shares for an answer only the identifier ranking contributed to.
    ///
    /// Fusion normalizes against the kinds that answered, so one positive
    /// share is the whole share and the other two never apply. The rank
    /// constant is the smallest accepted value rather than the operator's
    /// default, because with one input it cannot change an order: it divides
    /// every position by the same monotone factor.
    #[must_use]
    pub const fn identifier_only() -> Self {
        Self {
            identifier: 1.0,
            lexical: 0.0,
            vector: 0.0,
            fusion_k: FUSION_K_MIN,
        }
    }

    /// Names the three shares and the rank constant.
    ///
    /// # Errors
    ///
    /// Returns [`RankingError`] when a share is negative or is not a finite
    /// number, when every share is zero, or when `fusion_k` falls outside
    /// [`FUSION_K_MIN`] to [`FUSION_K_MAX`].
    pub fn new(
        identifier: f64,
        lexical: f64,
        vector: f64,
        fusion_k: u64,
    ) -> Result<Self, RankingError> {
        let shares = [identifier, lexical, vector];
        let bounded = shares
            .iter()
            .all(|share| share.is_finite() && (0.0..=1.0).contains(share));
        if !bounded || shares.iter().sum::<f64>() <= 0.0 {
            return Err(RankingError::new(
                RankingFault::new(RankingViolation::RankingWeightsInvalid).about("search.ranking"),
            ));
        }
        if !(FUSION_K_MIN..=FUSION_K_MAX).contains(&fusion_k) {
            return Err(RankingError::new(
                RankingFault::new(RankingViolation::FusionConstantInvalid)
                    .about("search.ranking.fusion_k"),
            ));
        }
        Ok(Self {
            identifier,
            lexical,
            vector,
            fusion_k,
        })
    }

    /// The share one input carries.
    #[must_use]
    pub const fn share(&self, kind: RankingInputKind) -> f64 {
        match kind {
            RankingInputKind::Identifier => self.identifier,
            RankingInputKind::Lexical => self.lexical,
            RankingInputKind::Vector => self.vector,
        }
    }

    /// The rank constant.
    #[must_use]
    pub const fn fusion_k(&self) -> u64 {
        self.fusion_k
    }
}

/// One fused result.
#[derive(Clone, Debug, PartialEq)]
pub struct FusedCandidate {
    identity: DocumentIdentity,
    fused: f64,
    score: f64,
    inputs: RankingInputSet,
    fields: FieldSet,
    phase: QueryPhase,
}

impl FusedCandidate {
    /// The identity this result belongs to.
    #[must_use]
    pub const fn identity(&self) -> &DocumentIdentity {
        &self.identity
    }

    /// The candidate's score, derived from its place in the final order.
    ///
    /// A score is comparable inside one answer and nowhere else: the same
    /// declaration answering two different queries carries two scores that
    /// mean nothing to each other.
    #[must_use]
    pub const fn score(&self) -> f64 {
        self.score
    }

    /// Every input that contributed to this candidate.
    #[must_use]
    pub const fn inputs(&self) -> RankingInputSet {
        self.inputs
    }

    /// Every field that placed this candidate.
    #[must_use]
    pub const fn fields(&self) -> FieldSet {
        self.fields
    }

    /// The phase that produced this candidate.
    #[must_use]
    pub const fn phase(&self) -> QueryPhase {
        self.phase
    }
}

/// One phase's fused answer, and whether the bound cut it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RankedCandidates {
    candidates: Vec<FusedCandidate>,
    truncated_at: Option<usize>,
}

impl RankedCandidates {
    /// The empty answer.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            candidates: Vec::new(),
            truncated_at: None,
        }
    }

    /// The ordered candidates, best first.
    #[must_use]
    pub fn candidates(&self) -> &[FusedCandidate] {
        &self.candidates
    }

    /// The ordered candidates, best first, owned.
    #[must_use]
    pub fn into_candidates(self) -> Vec<FusedCandidate> {
        self.candidates
    }

    /// Whether the answer holds no candidate.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// How many candidates the answer holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// The bound the answer stopped at while more candidates existed, or
    /// `None` when every candidate was kept.
    #[must_use]
    pub const fn truncated_at(&self) -> Option<usize> {
        self.truncated_at
    }

    /// Appends a later phase's answer after this one.
    ///
    /// An identity this answer already holds is dropped from `later`: the
    /// precise phase placed it, and a broad hit must never displace a precise
    /// one. Scores are derived again over the joined order, so a consumer
    /// reading scores alone sees the same sequence the order states.
    pub fn append_phase(&mut self, later: Self, keep_max: usize) {
        let held: Vec<&DocumentIdentity> = self
            .candidates
            .iter()
            .map(FusedCandidate::identity)
            .collect();
        let fresh: Vec<FusedCandidate> = later
            .candidates
            .into_iter()
            .filter(|candidate| !held.contains(&candidate.identity()))
            .collect();
        self.candidates.extend(fresh);
        if self.candidates.len() > keep_max {
            self.candidates.truncate(keep_max);
            self.truncated_at = Some(keep_max);
        } else if later.truncated_at.is_some() {
            self.truncated_at = self.truncated_at.or(later.truncated_at);
        }
        score_by_order(&mut self.candidates);
    }
}

/// Fuses `inputs` by weighted reciprocal rank.
///
/// Only inputs that answered take part, so an unavailable input contributes
/// nothing and leaves the order the remaining inputs state. An input that
/// placed nothing has not answered, so an index reaching for a kind and
/// finding none of it takes no share from the index that did find some.
/// Several selected indexes may each place a list of one kind; that kind's
/// share then splits evenly among them, so adding a second index does not
/// double what full-text matching is worth. Every candidate is
/// stamped with `phase` and with the fields and inputs that placed it. The
/// order is descending fused value, ties broken by identity ascending, cut to
/// `keep_max`.
///
/// No input at all, and inputs that all returned nothing, fuse to nothing
/// rather than refusing.
///
/// Work is one pass over each input's own slice plus one sort of the distinct
/// identities those slices named, so the whole call is bounded by what the
/// readers already bounded.
#[must_use]
pub fn fuse(
    inputs: &[RankingInput],
    weights: RankingWeights,
    phase: QueryPhase,
    keep_max: usize,
) -> RankedCandidates {
    let answering: Vec<&RankingInput> = inputs
        .iter()
        .filter(|input| input.answered() && weights.share(input.kind()) > 0.0)
        .collect();
    if answering.is_empty() {
        return RankedCandidates::empty();
    }
    let mut accumulated: BTreeMap<&DocumentIdentity, Accumulated> = BTreeMap::new();
    for input in &answering {
        let lists = as_count(
            answering
                .iter()
                .filter(|other| other.kind() == input.kind())
                .count(),
        );
        accumulate(
            &mut accumulated,
            input,
            weights.share(input.kind()) / lists,
            weights.fusion_k(),
        );
    }
    let mut candidates: Vec<FusedCandidate> = accumulated
        .into_iter()
        .map(|(identity, held)| FusedCandidate {
            identity: identity.clone(),
            fused: held.value,
            score: 0.0,
            inputs: held.inputs,
            fields: held.fields,
            phase,
        })
        .collect();
    candidates.sort_by(|left, right| {
        right
            .fused
            .total_cmp(&left.fused)
            .then_with(|| left.identity.cmp(&right.identity))
    });
    let truncated_at = (candidates.len() > keep_max).then_some(keep_max);
    candidates.truncate(keep_max);
    score_by_order(&mut candidates);
    RankedCandidates {
        candidates,
        truncated_at,
    }
}

/// What one identity accumulated across the inputs that ranked it.
#[derive(Clone, Copy, Debug, Default)]
struct Accumulated {
    value: f64,
    inputs: RankingInputSet,
    fields: FieldSet,
}

/// Adds one input's contribution to every identity it ranked.
///
/// A duplicate identity inside one input keeps its best position: the first
/// occurrence is the rank the formula reads, and a later repeat adds only its
/// fields. The rank counts the distinct identities seen so far, so a repeat
/// does not push the identities after it down a place.
fn accumulate<'a>(
    accumulated: &mut BTreeMap<&'a DocumentIdentity, Accumulated>,
    input: &'a RankingInput,
    share: f64,
    fusion_k: u64,
) {
    let mut ranked: Vec<&DocumentIdentity> = Vec::new();
    for entry in input.order() {
        let held = accumulated.entry(entry.identity()).or_default();
        held.inputs = held.inputs.with(input.kind());
        held.fields = held.fields.union(entry.fields());
        if ranked.contains(&entry.identity()) {
            continue;
        }
        ranked.push(entry.identity());
        held.value += share * reciprocal(fusion_k, ranked.len());
    }
}

/// The reciprocal-rank contribution of a 1-based position.
fn reciprocal(fusion_k: u64, rank: usize) -> f64 {
    1.0 / (as_float(fusion_k) + as_count(rank))
}

/// Widens the rank constant into the floating-point domain fusion works in.
///
/// [`RankingWeights::new`] refuses a `fusion_k` above [`FUSION_K_MAX`], so
/// the value is far below the exact-integer range and the conversion is
/// lossless.
#[expect(
    clippy::cast_precision_loss,
    reason = "fusion_k is bounded at FUSION_K_MAX"
)]
fn as_float(fusion_k: u64) -> f64 {
    fusion_k as f64
}

/// Derives every candidate's score from its place in the final order.
///
/// The best candidate scores 1.0 and each later one scores strictly less, so
/// a consumer reading scores alone sees exactly the sequence the order states
/// and never compares a broad hit against a precise hit from another answer.
fn score_by_order(candidates: &mut [FusedCandidate]) {
    let total = candidates.len();
    if total == 0 {
        return;
    }
    let total_value = as_count(total);
    for (position, candidate) in candidates.iter_mut().enumerate() {
        candidate.score = (total_value - as_count(position)) / total_value;
    }
}

/// Widens a bounded count into the floating-point domain a score is computed
/// in. Every count here is already bounded by the caller's `keep_max`.
#[expect(
    clippy::cast_precision_loss,
    reason = "every count here is a position the readers already bounded"
)]
fn as_count(value: usize) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::{
        FUSION_K_MAX, FusedCandidate, RankedCandidates, RankedIdentity, RankingInput,
        RankingInputKind, RankingInputSet, RankingWeights, fuse,
    };
    use crate::document::{DocumentIdentity, FieldSet, SearchableField};
    use crate::error::RankingViolation;
    use crate::query::QueryPhase;

    fn identity(value: &str) -> DocumentIdentity {
        DocumentIdentity::new(value).expect("identity must be accepted")
    }

    fn order(values: &[&str], field: SearchableField) -> Vec<RankedIdentity> {
        values
            .iter()
            .map(|value| RankedIdentity::new(identity(value), FieldSet::of(field)))
            .collect()
    }

    fn weights() -> RankingWeights {
        RankingWeights::new(0.35, 0.35, 0.30, 60).expect("weights must be accepted")
    }

    /// Whether two scores agree within the last bits a literal can carry.
    fn close(computed: f64, expected: f64) -> bool {
        (computed - expected).abs() < 1e-12
    }

    fn score(candidate: &FusedCandidate) -> f64 {
        candidate.score()
    }

    fn identities(ranked: &RankedCandidates) -> Vec<&str> {
        ranked
            .candidates()
            .iter()
            .map(|candidate| candidate.identity().as_str())
            .collect()
    }

    #[test]
    fn test_a_negative_share_is_refused() {
        assert_eq!(
            RankingWeights::new(-0.1, 0.5, 0.5, 60)
                .expect_err("a negative share must be refused")
                .fault()
                .violation(),
            RankingViolation::RankingWeightsInvalid
        );
    }

    #[test]
    fn test_a_share_above_one_is_refused() {
        assert!(RankingWeights::new(1.1, 0.0, 0.0, 60).is_err());
    }

    #[test]
    fn test_a_share_that_is_not_a_number_is_refused() {
        assert!(RankingWeights::new(f64::NAN, 0.5, 0.5, 60).is_err());
    }

    #[test]
    fn test_three_zero_shares_are_refused() {
        assert!(RankingWeights::new(0.0, 0.0, 0.0, 60).is_err());
    }

    #[test]
    fn test_a_rank_constant_outside_the_range_is_refused() {
        assert_eq!(
            RankingWeights::new(0.5, 0.5, 0.0, 0)
                .expect_err("a zero rank constant must be refused")
                .fault()
                .violation(),
            RankingViolation::FusionConstantInvalid
        );
        assert!(RankingWeights::new(0.5, 0.5, 0.0, FUSION_K_MAX + 1).is_err());
    }

    #[test]
    fn test_one_share_carries_its_own_rank_constant_and_shares() {
        let held = weights();
        assert!(close(held.share(RankingInputKind::Identifier), 0.35));
        assert!(close(held.share(RankingInputKind::Lexical), 0.35));
        assert!(close(held.share(RankingInputKind::Vector), 0.30));
        assert_eq!(held.fusion_k(), 60);
    }

    #[test]
    fn test_no_input_fuses_to_nothing() {
        assert!(fuse(&[], weights(), QueryPhase::Precise, 10).is_empty());
    }

    #[test]
    fn test_inputs_that_returned_nothing_fuse_to_nothing() {
        let inputs = [
            RankingInput::unanswered(RankingInputKind::Identifier),
            RankingInput::new(RankingInputKind::Lexical, Vec::new()),
        ];
        assert!(fuse(&inputs, weights(), QueryPhase::Precise, 10).is_empty());
    }

    #[test]
    fn test_an_input_whose_share_is_zero_does_not_take_part() {
        let held = RankingWeights::new(0.5, 0.5, 0.0, 60).expect("weights must be accepted");
        let inputs = [RankingInput::new(
            RankingInputKind::Vector,
            order(&["only"], SearchableField::Name),
        )];
        assert!(fuse(&inputs, held, QueryPhase::Precise, 10).is_empty());
    }

    #[test]
    fn test_a_candidate_every_input_ranked_first_scores_one() {
        let inputs = [
            RankingInput::new(
                RankingInputKind::Identifier,
                order(&["a"], SearchableField::Name),
            ),
            RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a"], SearchableField::QualifiedName),
            ),
        ];
        let ranked = fuse(&inputs, weights(), QueryPhase::Precise, 10);
        assert_eq!(ranked.len(), 1);
        assert!(close(ranked.candidates()[0].score(), 1.0));
    }

    #[test]
    fn test_a_candidate_records_every_input_and_field_that_placed_it() {
        let inputs = [
            RankingInput::new(
                RankingInputKind::Identifier,
                order(&["a"], SearchableField::Name),
            ),
            RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a"], SearchableField::Documentation),
            ),
        ];
        let ranked = fuse(&inputs, weights(), QueryPhase::Precise, 10);
        let candidate = &ranked.candidates()[0];
        assert_eq!(
            candidate.inputs(),
            RankingInputSet::of(RankingInputKind::Identifier).with(RankingInputKind::Lexical)
        );
        assert!(candidate.fields().holds(SearchableField::Name));
        assert!(candidate.fields().holds(SearchableField::Documentation));
        assert_eq!(candidate.phase(), QueryPhase::Precise);
    }

    #[test]
    fn test_a_missing_input_leaves_the_remaining_order_unchanged() {
        let identifier = RankingInput::new(
            RankingInputKind::Identifier,
            order(&["a", "b", "c"], SearchableField::Name),
        );
        let lexical = RankingInput::new(
            RankingInputKind::Lexical,
            order(&["b", "a"], SearchableField::QualifiedName),
        );
        let without = fuse(
            &[identifier.clone(), lexical.clone()],
            weights(),
            QueryPhase::Precise,
            10,
        );
        let with_empty = fuse(
            &[
                identifier,
                lexical,
                RankingInput::unanswered(RankingInputKind::Vector),
            ],
            weights(),
            QueryPhase::Precise,
            10,
        );
        assert_eq!(identities(&without), identities(&with_empty));
        assert_eq!(identities(&without), ["a", "b", "c"]);
        assert_eq!(
            without.candidates().iter().map(score).collect::<Vec<f64>>(),
            with_empty
                .candidates()
                .iter()
                .map(score)
                .collect::<Vec<f64>>()
        );
    }

    #[test]
    fn test_an_input_that_answers_does_move_the_order() {
        // The negative space of the case above: the vector input is the only
        // difference between the two calls, so an order that stays the same
        // when it answers would mean fusion ignored it either way.
        let identifier = RankingInput::new(
            RankingInputKind::Identifier,
            order(&["a", "b", "c"], SearchableField::Name),
        );
        let lexical = RankingInput::new(
            RankingInputKind::Lexical,
            order(&["b", "a"], SearchableField::QualifiedName),
        );
        let vector = RankingInput::new(
            RankingInputKind::Vector,
            order(&["c"], SearchableField::DeclarationSource),
        );
        let weights = RankingWeights::new(0.1, 0.1, 0.8, 1).expect("weights must be accepted");
        let without = fuse(
            &[identifier.clone(), lexical.clone()],
            weights,
            QueryPhase::Precise,
            10,
        );
        let answered = fuse(
            &[identifier, lexical, vector],
            weights,
            QueryPhase::Precise,
            10,
        );
        assert_eq!(identities(&without), ["a", "b", "c"]);
        assert_eq!(identities(&answered), ["c", "a", "b"]);
    }

    #[test]
    fn test_a_repeat_does_not_push_the_identities_after_it_down() {
        // The rank the formula reads counts distinct identities, so the repeat
        // of `a` must leave `b` at rank two. Reading the raw position instead
        // would rank `b` third and lose it to a competitor ranked second by
        // another input.
        let repeated = fuse(
            &[RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a", "a", "b"], SearchableField::Name),
            )],
            weights(),
            QueryPhase::Precise,
            10,
        );
        let distinct = fuse(
            &[RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a", "b"], SearchableField::Name),
            )],
            weights(),
            QueryPhase::Precise,
            10,
        );
        assert_eq!(identities(&repeated), ["a", "b"]);
        assert!(close(
            repeated.candidates()[1].fused,
            distinct.candidates()[1].fused
        ));
    }

    #[test]
    fn test_equal_fused_values_order_by_identity() {
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            vec![
                RankedIdentity::new(identity("b"), FieldSet::of(SearchableField::Name)),
                RankedIdentity::new(identity("a"), FieldSet::of(SearchableField::Name)),
            ],
        )];
        let ranked = fuse(&inputs, weights(), QueryPhase::Precise, 10);
        assert_eq!(identities(&ranked), ["b", "a"]);
        let tied = [
            RankingInput::new(
                RankingInputKind::Identifier,
                order(&["b"], SearchableField::Name),
            ),
            RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a"], SearchableField::Name),
            ),
        ];
        assert_eq!(
            identities(&fuse(&tied, weights(), QueryPhase::Precise, 10)),
            ["a", "b"]
        );
    }

    #[test]
    fn test_a_duplicate_identity_inside_one_input_keeps_its_best_position() {
        let repeated = RankingInput::new(
            RankingInputKind::Lexical,
            vec![
                RankedIdentity::new(identity("a"), FieldSet::of(SearchableField::Name)),
                RankedIdentity::new(identity("b"), FieldSet::of(SearchableField::Name)),
                RankedIdentity::new(identity("a"), FieldSet::of(SearchableField::FileContent)),
            ],
        );
        let ranked = fuse(&[repeated], weights(), QueryPhase::Precise, 10);
        assert_eq!(identities(&ranked), ["a", "b"]);
        assert!(
            ranked.candidates()[0]
                .fields()
                .holds(SearchableField::FileContent),
            "a repeat still contributes the field it matched through"
        );
    }

    #[test]
    fn test_fusion_cuts_to_its_bound_and_reports_the_cut() {
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            order(&["a", "b", "c"], SearchableField::Name),
        )];
        let ranked = fuse(&inputs, weights(), QueryPhase::Precise, 2);
        assert_eq!(identities(&ranked), ["a", "b"]);
        assert_eq!(ranked.truncated_at(), Some(2));
    }

    #[test]
    fn test_an_answer_within_its_bound_reports_no_cut() {
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            order(&["a", "b"], SearchableField::Name),
        )];
        assert_eq!(
            fuse(&inputs, weights(), QueryPhase::Precise, 2).truncated_at(),
            None
        );
    }

    #[test]
    fn test_scores_descend_with_the_final_order() {
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            order(&["a", "b", "c"], SearchableField::Name),
        )];
        let ranked = fuse(&inputs, weights(), QueryPhase::Precise, 10);
        let scores: Vec<f64> = ranked
            .candidates()
            .iter()
            .map(super::FusedCandidate::score)
            .collect();
        assert!(scores.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(close(scores[0], 1.0));
    }

    #[test]
    fn test_a_broad_phase_appends_after_precise_and_drops_what_precise_held() {
        let precise = fuse(
            &[RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a", "b"], SearchableField::Name),
            )],
            weights(),
            QueryPhase::Precise,
            10,
        );
        let broad = fuse(
            &[RankingInput::new(
                RankingInputKind::Lexical,
                order(&["c", "a", "d"], SearchableField::FileContent),
            )],
            weights(),
            QueryPhase::Broad,
            10,
        );
        let mut joined = precise;
        joined.append_phase(broad, 10);
        assert_eq!(identities(&joined), ["a", "b", "c", "d"]);
        assert_eq!(joined.candidates()[0].phase(), QueryPhase::Precise);
        assert_eq!(joined.candidates()[2].phase(), QueryPhase::Broad);
        let scores: Vec<f64> = joined
            .candidates()
            .iter()
            .map(super::FusedCandidate::score)
            .collect();
        assert!(scores.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn test_appending_a_phase_cuts_to_the_bound_and_reports_the_cut() {
        let mut joined = fuse(
            &[RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a"], SearchableField::Name),
            )],
            weights(),
            QueryPhase::Precise,
            10,
        );
        joined.append_phase(
            fuse(
                &[RankingInput::new(
                    RankingInputKind::Lexical,
                    order(&["b", "c"], SearchableField::Name),
                )],
                weights(),
                QueryPhase::Broad,
                10,
            ),
            2,
        );
        assert_eq!(identities(&joined), ["a", "b"]);
        assert_eq!(joined.truncated_at(), Some(2));
    }

    #[test]
    fn test_two_indexes_of_one_kind_split_that_kinds_share() {
        let held = RankingWeights::new(0.5, 0.5, 0.0, 60).expect("weights must be accepted");
        let identifier = RankingInput::new(
            RankingInputKind::Identifier,
            order(&["a"], SearchableField::Name),
        );
        let one_lexical = RankingInput::new(
            RankingInputKind::Lexical,
            order(&["b"], SearchableField::Name),
        );
        let other_lexical = RankingInput::new(
            RankingInputKind::Lexical,
            order(&["b"], SearchableField::Name),
        );
        let one_index = fuse(
            &[identifier.clone(), one_lexical.clone()],
            held,
            QueryPhase::Precise,
            10,
        );
        let two_indexes = fuse(
            &[identifier, one_lexical, other_lexical],
            held,
            QueryPhase::Precise,
            10,
        );
        assert_eq!(identities(&one_index), ["a", "b"]);
        assert_eq!(
            identities(&two_indexes),
            ["a", "b"],
            "a second index of one kind splits that kind's share rather than \
             doubling what full-text matching is worth"
        );
    }

    #[test]
    fn test_one_input_alone_carries_the_whole_share() {
        let inputs = [RankingInput::new(
            RankingInputKind::Identifier,
            order(&["a"], SearchableField::Name),
        )];
        let ranked = fuse(
            &inputs,
            RankingWeights::identifier_only(),
            QueryPhase::Precise,
            10,
        );
        assert_eq!(identities(&ranked), ["a"]);
        assert!(close(ranked.candidates()[0].score(), 1.0));
    }

    #[test]
    fn test_an_empty_answer_reports_itself_empty() {
        let empty = RankedCandidates::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.truncated_at(), None);
        assert!(empty.into_candidates().is_empty());
    }

    #[test]
    fn test_an_input_set_collects_and_lists_its_members() {
        let set: RankingInputSet = [RankingInputKind::Vector, RankingInputKind::Identifier]
            .into_iter()
            .collect();
        assert_eq!(
            set.kinds().collect::<Vec<_>>(),
            [RankingInputKind::Identifier, RankingInputKind::Vector]
        );
        assert!(!set.holds(RankingInputKind::Lexical));
        assert!(RankingInputSet::EMPTY.is_empty());
    }

    #[test]
    fn test_each_input_kind_names_itself() {
        assert_eq!(RankingInputKind::Identifier.label(), "identifier");
        assert_eq!(RankingInputKind::Lexical.label(), "lexical");
        assert_eq!(RankingInputKind::Vector.label(), "vector");
    }

    #[test]
    fn test_an_input_reports_what_it_holds() {
        let input = RankingInput::new(
            RankingInputKind::Lexical,
            order(&["a"], SearchableField::Name),
        );
        assert_eq!(input.kind(), RankingInputKind::Lexical);
        assert_eq!(input.order().len(), 1);
        assert!(input.answered());
        assert!(!RankingInput::unanswered(RankingInputKind::Vector).answered());
    }
}
