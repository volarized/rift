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
//! resolves identities after ranking, which is what lets the global route order
//! package hits beside project hits through this same code.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::document::{DocumentIdentity, FieldSet, SearchableField};
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

/// One identity an input ranked, the fields that placed it, and, for a row holding
/// file text, which bytes of its file that row holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RankedIdentity {
    identity: DocumentIdentity,
    fields: FieldSet,
    file_range: Option<Range<u64>>,
}

impl RankedIdentity {
    /// Names one ranked identity and the fields that matched.
    #[must_use]
    pub const fn new(identity: DocumentIdentity, fields: FieldSet) -> Self {
        Self {
            identity,
            fields,
            file_range: None,
        }
    }

    /// The same identity, holding the bytes `file_range` of its file: the whole file, or
    /// the one chunk of a large file this row stores.
    #[must_use]
    pub fn with_file_range(mut self, file_range: Range<u64>) -> Self {
        self.file_range = Some(file_range);
        self
    }

    /// The bytes of its file this row holds, or `None` for a row holding no file text.
    #[must_use]
    pub const fn file_range(&self) -> Option<&Range<u64>> {
        self.file_range.as_ref()
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
        let Some(violation) = weights_violation(identifier, lexical, vector, fusion_k) else {
            return Ok(Self {
                identifier,
                lexical,
                vector,
                fusion_k,
            });
        };
        let subject = match violation {
            RankingViolation::FusionConstantInvalid => "search.ranking.fusion_k",
            _ => "search.ranking",
        };
        Err(RankingError::new(
            RankingFault::new(violation).about(subject),
        ))
    }

    /// Names shares and a rank constant a caller writes in its own source.
    ///
    /// Evaluated in a `const` item, an invalid set fails to compile, so the caller holds
    /// its weights without a runtime refusal to handle.
    ///
    /// # Panics
    ///
    /// Panics on the values [`Self::new`] refuses, which in a `const` item is a compile
    /// error.
    #[must_use]
    pub const fn fixed(identifier: f64, lexical: f64, vector: f64, fusion_k: u64) -> Self {
        assert!(
            weights_violation(identifier, lexical, vector, fusion_k).is_none(),
            "fixed ranking weights need shares from 0 to 1 with a positive sum and a rank \
             constant from FUSION_K_MIN to FUSION_K_MAX"
        );
        Self {
            identifier,
            lexical,
            vector,
            fusion_k,
        }
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

/// The rule a set of shares and a rank constant breaks, if any: every share a finite
/// number from 0 to 1, their sum positive, and the rank constant from [`FUSION_K_MIN`] to
/// [`FUSION_K_MAX`]. [`RankingWeights::new`] refuses what this names, and
/// [`RankingWeights::fixed`] refuses it at compile time.
const fn weights_violation(
    identifier: f64,
    lexical: f64,
    vector: f64,
    fusion_k: u64,
) -> Option<RankingViolation> {
    let shares_bounded =
        share_bounded(identifier) && share_bounded(lexical) && share_bounded(vector);
    let shares_positive = identifier + lexical + vector > 0.0;
    let constant_bounded = FUSION_K_MIN <= fusion_k && fusion_k <= FUSION_K_MAX;
    match (shares_bounded && shares_positive, constant_bounded) {
        (false, _) => Some(RankingViolation::RankingWeightsInvalid),
        (true, false) => Some(RankingViolation::FusionConstantInvalid),
        (true, true) => None,
    }
}

/// Whether one share is a finite number from 0 to 1.
const fn share_bounded(share: f64) -> bool {
    share.is_finite() && 0.0 <= share && share <= 1.0
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
    file_range: Option<Range<u64>>,
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

    /// The bytes of its file this candidate's row holds, as the input that ranked it
    /// stated them; `None` for a candidate holding no file text.
    #[must_use]
    pub const fn file_range(&self) -> Option<&Range<u64>> {
        self.file_range.as_ref()
    }

    /// A declaration a body match placed: found inside `row`'s text, so it answers with
    /// the fields and the phase that placed that row.
    fn body_match(identity: DocumentIdentity, row: &Self) -> Self {
        Self {
            identity,
            fused: row.fused,
            score: 0.0,
            inputs: row.inputs,
            fields: FieldSet::of(SearchableField::FileContent),
            phase: row.phase,
            file_range: None,
        }
    }
}

/// Whether an answer keeps its file rows beside the declarations a body match placed
/// from them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRowAnswer {
    /// The answer names files too, so every file row keeps its place.
    Kept,
    /// The answer names declarations alone, so a file row answers only through the
    /// declarations found inside it.
    Dropped,
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

    /// Places declarations found inside file rows at the first file row's place, best
    /// first.
    ///
    /// `body_matches` is the pool a body match scored together, best first. Every
    /// candidate before the first file row keeps its place; the pool follows it, then that
    /// file row when `file_rows` keeps it, then every later candidate in its own order. A
    /// declaration keeps the first place it reaches, so a pooled declaration the answer
    /// already held earlier stays there, and a later direct match of a pooled
    /// declaration moves up to the pool's place. The joined answer is cut to `keep_max`,
    /// and its scores are derived again over the joined order.
    #[must_use]
    pub fn with_body_matches(
        self,
        body_matches: Vec<DocumentIdentity>,
        is_file_row: impl Fn(&FusedCandidate) -> bool,
        file_rows: FileRowAnswer,
        keep_max: usize,
    ) -> Self {
        let mut placed: BTreeSet<DocumentIdentity> = BTreeSet::new();
        let mut candidates = Vec::with_capacity(self.candidates.len() + body_matches.len());
        let mut pool = Some(body_matches);
        for candidate in self.candidates {
            if !is_file_row(&candidate) {
                if placed.insert(candidate.identity.clone()) {
                    candidates.push(candidate);
                }
                continue;
            }
            for identity in pool.take().into_iter().flatten() {
                if placed.insert(identity.clone()) {
                    candidates.push(FusedCandidate::body_match(identity, &candidate));
                }
            }
            if file_rows == FileRowAnswer::Kept {
                candidates.push(candidate);
            }
        }
        let mut truncated_at = self.truncated_at;
        if candidates.len() > keep_max {
            candidates.truncate(keep_max);
            truncated_at = Some(keep_max);
        }
        score_by_order(&mut candidates);
        Self {
            candidates,
            truncated_at,
        }
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
            file_range: held.file_range,
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
#[derive(Clone, Debug, Default)]
struct Accumulated {
    value: f64,
    inputs: RankingInputSet,
    fields: FieldSet,
    file_range: Option<Range<u64>>,
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
        if held.file_range.is_none() {
            held.file_range = entry.file_range().cloned();
        }
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
        FUSION_K_MAX, FileRowAnswer, FusedCandidate, RankedCandidates, RankedIdentity,
        RankingInput, RankingInputKind, RankingInputSet, RankingWeights, fuse,
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

    /// One lexical answer in exactly `values`' order, the file rows holding `0..64` of
    /// their file.
    fn lexical_answer(values: &[&str]) -> RankedCandidates {
        let order = values
            .iter()
            .map(|value| {
                let ranked =
                    RankedIdentity::new(identity(value), FieldSet::of(SearchableField::Name));
                if value.starts_with("file") {
                    ranked.with_file_range(0..64)
                } else {
                    ranked
                }
            })
            .collect();
        fuse(
            &[RankingInput::new(RankingInputKind::Lexical, order)],
            weights(),
            QueryPhase::Broad,
            100,
        )
    }

    fn is_file_row(candidate: &FusedCandidate) -> bool {
        candidate.file_range().is_some()
    }

    /// The pool takes the first file row's place, best first: a direct match placed
    /// before that row keeps its own place, a later direct match of a pooled declaration
    /// moves up to the pool's place, and a declaration-only answer drops every file row.
    #[test]
    fn test_body_matches_take_the_first_file_row_place() {
        let answer = lexical_answer(&["alpha", "file:a.rs", "beta", "file:b.rs", "gamma"]);
        let pooled = vec![identity("delta"), identity("gamma"), identity("alpha")];
        let placed = answer.clone().with_body_matches(
            pooled.clone(),
            is_file_row,
            FileRowAnswer::Dropped,
            100,
        );
        assert_eq!(identities(&placed), ["alpha", "delta", "gamma", "beta"]);
        let delta = &placed.candidates()[1];
        assert!(delta.fields().holds(SearchableField::FileContent));
        assert_eq!(delta.phase(), QueryPhase::Broad);
        assert_eq!(delta.file_range(), None);
        assert!(delta.inputs().holds(RankingInputKind::Lexical));
        let scores: Vec<f64> = placed
            .candidates()
            .iter()
            .map(FusedCandidate::score)
            .collect();
        assert!(
            scores.windows(2).all(|pair| pair[0] > pair[1]),
            "{scores:?}"
        );

        let kept = answer.with_body_matches(pooled, is_file_row, FileRowAnswer::Kept, 100);
        assert_eq!(
            identities(&kept),
            ["alpha", "delta", "gamma", "file:a.rs", "beta", "file:b.rs"]
        );
    }

    /// An answer with no file row places nothing, and one past `keep_max` is cut and says
    /// so.
    #[test]
    fn test_body_matches_without_a_file_row_place_nothing_and_the_bound_cuts() {
        let answer = lexical_answer(&["alpha", "beta"]);
        let unchanged = answer.clone().with_body_matches(
            vec![identity("delta")],
            is_file_row,
            FileRowAnswer::Dropped,
            100,
        );
        assert_eq!(identities(&unchanged), ["alpha", "beta"]);
        assert_eq!(unchanged.truncated_at(), None);

        let answer = lexical_answer(&["file:a.rs", "alpha"]);
        let cut = answer.with_body_matches(
            vec![identity("delta"), identity("epsilon")],
            is_file_row,
            FileRowAnswer::Kept,
            3,
        );
        assert_eq!(identities(&cut), ["delta", "epsilon", "file:a.rs"]);
        assert_eq!(cut.truncated_at(), Some(3));
    }

    #[test]
    fn test_fusion_keeps_the_file_range_an_input_stated() {
        let answer = lexical_answer(&["file:a.rs", "alpha"]);
        assert_eq!(answer.candidates()[0].file_range(), Some(&(0..64)));
        assert_eq!(answer.candidates()[1].file_range(), None);
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

    /// A weight set fixed in a `const` item, or at run time, holds the values `new` accepts
    /// for it.
    #[test]
    fn test_fixed_weights_equal_the_accepted_set() {
        const FIXED: RankingWeights = RankingWeights::fixed(0.35, 0.35, 0.30, 60);
        assert_eq!(FIXED, weights());
        assert_eq!(RankingWeights::fixed(0.35, 0.35, 0.30, 60), weights());
    }

    /// Outside a `const` item, a set `new` refuses panics naming the rule it broke.
    #[test]
    #[should_panic(expected = "fixed ranking weights need shares from 0 to 1")]
    fn test_fixed_weights_refuse_shares_summing_to_zero() {
        let _ = RankingWeights::fixed(0.0, 0.0, 0.0, 60);
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
            order(&["c"], SearchableField::FileContent),
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
    fn test_a_later_phase_that_was_cut_says_so_even_when_the_join_fits() {
        // The joined answer is inside `keep_max`, so the join cut nothing. What
        // the broad phase cut is still gone, and a caller that reads only the
        // joined answer would otherwise never learn it.
        let mut precise = fuse(
            &[RankingInput::new(
                RankingInputKind::Lexical,
                order(&["a"], SearchableField::Name),
            )],
            weights(),
            QueryPhase::Precise,
            10,
        );
        assert!(precise.truncated_at().is_none());
        let later = fuse(
            &[RankingInput::new(
                RankingInputKind::Lexical,
                order(&["b", "c", "d"], SearchableField::Name),
            )],
            weights(),
            QueryPhase::Broad,
            1,
        );
        assert_eq!(later.truncated_at(), Some(1));
        precise.append_phase(later, 10);
        assert_eq!(identities(&precise), ["a", "b"]);
        assert_eq!(
            precise.truncated_at(),
            Some(1),
            "what the broad phase cut is still cut"
        );
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
