//! One index over both search tiers, and how far the semantic tier has got.
//!
//! [`SearchIndex`] owns the lexical index and the vector store against one
//! database file, so a caller drives search through it and never opens either
//! store itself. One `search` runs both tiers and fuses what they returned.
//!
//! Nothing here spawns a task, blocks on a runtime, or knows what a server is.
//! The caller drives [`SearchIndex::prepare`], [`SearchIndex::build`], and
//! [`SearchIndex::refresh`] on whatever task it likes. The encoder's forward
//! pass runs on the calling task, so a caller that must keep a runtime worker
//! free places these calls where blocking work is allowed.
//!
//! What the semantic tier can answer right now is [`SemanticReadiness`]. This
//! crate reports it and stops there: the wire warning a caller attaches to a
//! result is built above, by the layer that owns the protocol models, because
//! the search tier sits below that layer and never depends on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rift_core::ProjectPath;
use rift_index::{
    LexicalIndexError, LexicalIndexLimits, LexicalMatch, LexicalSearchIndex, LexicalUnit,
    LexicalUnitKind, SemanticVectorStore, StoredVector,
};

use crate::acquisition::{AcquisitionLimits, ModelSource, acquire};
use crate::document::{Declaration, digests, document};
use crate::encoder::{Encoder, EncoderLimits};
use crate::error::{SearchError, SearchFault, SearchViolation};
use crate::fusion::{DeclarationMatch, FusedRank, Ranking, fuse, spread_per_file};
use crate::similarity::{SemanticMatch, nearest};

/// The lexical ranking's share of a fused score when the caller sets none.
const LEXICAL_WEIGHT_DEFAULT: f64 = 0.7;
/// The semantic ranking's share of a fused score when the caller sets none.
const SEMANTIC_WEIGHT_DEFAULT: f64 = 0.3;
/// The reciprocal-rank constant when the caller sets none.
const FUSION_K_DEFAULT: u64 = 60;
/// Declarations the semantic ranking returns when the caller sets none.
const CANDIDATES_DEFAULT: u64 = 200;
/// Vectors the workspace may hold when the caller sets none.
const MAX_VECTORS_DEFAULT: u64 = 200_000;
/// Declarations one embedding pass takes when the caller sets none.
const BATCH_DECLARATIONS_DEFAULT: u64 = 32;
/// Tokens the encoder reads from one declaration when the caller sets none.
const MAX_TOKENS_DEFAULT: u64 = 256;
/// Candidates one file may contribute when the caller sets none.
const PER_FILE_MAX_DEFAULT: u64 = 3;

/// How far the semantic tier has got.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticReadiness {
    /// No semantic tier: the workspace turned it off.
    Disabled,
    /// Embedding is under way.
    Preparing {
        /// Declarations that already carry a vector.
        prepared: u64,
        /// Declarations the published set holds.
        total: u64,
    },
    /// Every declaration in the published set has a vector.
    Ready,
    /// The tier will not answer for the life of this index.
    Unavailable,
}

impl SemanticReadiness {
    /// Whether the tier may take part in a ranking.
    ///
    /// `Preparing` counts: what is already embedded ranks, and a partial
    /// ranking beside the lexical one is worth more than none.
    const fn answers(self) -> bool {
        matches!(self, Self::Ready | Self::Preparing { .. })
    }
}

/// One unit both tiers can return, and the fused score that ranked it.
#[derive(Clone, Debug, PartialEq)]
pub struct RankedUnit {
    identity: String,
    path: ProjectPath,
    kind: LexicalUnitKind,
    score: f64,
}

impl RankedUnit {
    /// The ranked unit's stable identity.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// The project-relative path the unit lives at.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// The unit's granularity.
    #[must_use]
    pub const fn kind(&self) -> LexicalUnitKind {
        self.kind
    }

    /// The fused score, `1.0` for a unit both tiers put first.
    #[must_use]
    pub const fn score(&self) -> f64 {
        self.score
    }
}

/// What one [`SearchIndex`] may spend, and how it weighs its two tiers.
///
/// This is the search tier's own type. The layer that reads the workspace
/// configuration translates the operator's keys into it, exactly as it does
/// for [`LexicalIndexLimits`], so this crate stays below the protocol models.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchIndexLimits {
    lexical: LexicalIndexLimits,
    lexical_weight: f64,
    semantic_weight: f64,
    fusion_k: u64,
    candidates: u64,
    max_vectors: u64,
    batch_declarations: u64,
    max_tokens: u64,
    per_file_max: u64,
    semantic_disabled: bool,
}

impl SearchIndexLimits {
    /// Starts from the lexical bounds both stores open under.
    pub const fn builder(lexical: LexicalIndexLimits) -> SearchIndexLimitsBuilder {
        SearchIndexLimitsBuilder {
            limits: Self {
                lexical,
                lexical_weight: LEXICAL_WEIGHT_DEFAULT,
                semantic_weight: SEMANTIC_WEIGHT_DEFAULT,
                fusion_k: FUSION_K_DEFAULT,
                candidates: CANDIDATES_DEFAULT,
                max_vectors: MAX_VECTORS_DEFAULT,
                batch_declarations: BATCH_DECLARATIONS_DEFAULT,
                max_tokens: MAX_TOKENS_DEFAULT,
                per_file_max: PER_FILE_MAX_DEFAULT,
                semantic_disabled: false,
            },
        }
    }

    /// The bounds both stores open under.
    #[must_use]
    pub const fn lexical(self) -> LexicalIndexLimits {
        self.lexical
    }

    /// The lexical ranking's share of a fused score.
    #[must_use]
    pub const fn lexical_weight(self) -> f64 {
        self.lexical_weight
    }

    /// The semantic ranking's share of a fused score.
    #[must_use]
    pub const fn semantic_weight(self) -> f64 {
        self.semantic_weight
    }

    /// The reciprocal-rank constant fusion flattens each ranking's head with.
    #[must_use]
    pub const fn fusion_k(self) -> u64 {
        self.fusion_k
    }

    /// Declarations the semantic ranking returns before the two are fused.
    #[must_use]
    pub const fn candidates(self) -> u64 {
        self.candidates
    }

    /// Vectors the workspace may hold.
    #[must_use]
    pub const fn max_vectors(self) -> u64 {
        self.max_vectors
    }

    /// Declarations one embedding pass hands the encoder.
    #[must_use]
    pub const fn batch_declarations(self) -> u64 {
        self.batch_declarations
    }

    /// Tokens the encoder reads from one declaration.
    #[must_use]
    pub const fn max_tokens(self) -> u64 {
        self.max_tokens
    }

    /// Candidates one file may contribute to the semantic ranking.
    #[must_use]
    pub const fn per_file_max(self) -> u64 {
        self.per_file_max
    }

    /// Whether the workspace turned the semantic tier off.
    #[must_use]
    pub const fn is_semantic_disabled(self) -> bool {
        self.semantic_disabled
    }

    /// The bounds one encoder loads under.
    ///
    /// One call carries one pass: the embedding loop chunks by
    /// `batch_declarations` itself, so the encoder's own text bound is the
    /// same number and a batch can never reach its refusal.
    fn encoder_limits(self) -> EncoderLimits {
        let batch = batch_size(self.batch_declarations);
        EncoderLimits::new(batch, as_usize(self.max_tokens), batch)
    }

    /// The readiness one freshly opened index starts at.
    const fn initial_readiness(self) -> SemanticReadiness {
        if self.semantic_disabled {
            SemanticReadiness::Disabled
        } else {
            SemanticReadiness::Preparing {
                prepared: 0,
                total: 0,
            }
        }
    }

    /// How deep the semantic ranking is read before it is spread across files.
    ///
    /// A file may keep `per_file_max` candidates, so reading the ranking that
    /// many times deeper than the candidate list leaves a full list even when
    /// one file's declarations hold the whole head.
    fn depth(self) -> usize {
        as_usize(self.candidates.saturating_mul(self.per_file_max))
    }
}

impl Default for SearchIndexLimits {
    /// The shipped bounds: the lexical defaults, a 0.7 and 0.3 weight pair,
    /// a rank constant of 60, 200 semantic candidates spread 3 per file, and
    /// 200,000 vectors embedded 32 at a time over 256 tokens each.
    fn default() -> Self {
        Self::builder(LexicalIndexLimits::default()).build()
    }
}

/// Builds one [`SearchIndexLimits`], starting from the shipped bounds.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub struct SearchIndexLimitsBuilder {
    limits: SearchIndexLimits,
}

impl SearchIndexLimitsBuilder {
    /// Sets the share each ranking carries of a fused score.
    ///
    /// The pair is set together because it is one decision: the two trade
    /// against each other, and fusion refuses a pair that cannot carry a
    /// score.
    pub const fn weights(mut self, lexical: f64, semantic: f64) -> Self {
        self.limits.lexical_weight = lexical;
        self.limits.semantic_weight = semantic;
        self
    }

    /// Sets the reciprocal-rank constant.
    pub const fn fusion_k(mut self, fusion_k: u64) -> Self {
        self.limits.fusion_k = fusion_k;
        self
    }

    /// Sets how many declarations the semantic ranking returns.
    pub const fn candidates(mut self, candidates: u64) -> Self {
        self.limits.candidates = candidates;
        self
    }

    /// Sets how many vectors the workspace may hold.
    pub const fn max_vectors(mut self, max_vectors: u64) -> Self {
        self.limits.max_vectors = max_vectors;
        self
    }

    /// Sets how many declarations one embedding pass takes.
    pub const fn batch_declarations(mut self, batch_declarations: u64) -> Self {
        self.limits.batch_declarations = batch_declarations;
        self
    }

    /// Sets how many tokens the encoder reads from one declaration.
    pub const fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.limits.max_tokens = max_tokens;
        self
    }

    /// Sets how many candidates one file may contribute.
    pub const fn per_file_max(mut self, per_file_max: u64) -> Self {
        self.limits.per_file_max = per_file_max;
        self
    }

    /// Turns the semantic tier off, so nothing is acquired and nothing is
    /// embedded.
    pub const fn disable_semantic(mut self) -> Self {
        self.limits.semantic_disabled = true;
        self
    }

    /// The finished bounds.
    ///
    /// Nothing is validated here. The weights and the rank constant are
    /// refused by [`fuse`] itself, which is the one place that reads them,
    /// and a second check would be a second representation of that rule.
    #[must_use]
    pub const fn build(self) -> SearchIndexLimits {
        self.limits
    }
}

/// The loaded encoder and the identity its vectors are addressed under.
#[derive(Debug)]
struct LoadedModel {
    identity: String,
    encoder: Encoder,
}

/// Where one ranked unit lives, without the content either store holds.
#[derive(Clone, Debug)]
struct UnitAddress {
    identity: String,
    path: ProjectPath,
    kind: LexicalUnitKind,
}

impl UnitAddress {
    /// The address of one indexed unit.
    fn of(unit: &LexicalUnit) -> Self {
        Self {
            identity: unit.identity().to_owned(),
            path: unit.path().clone(),
            kind: unit.kind(),
        }
    }
}

/// One lexical unit together with the declaration whose text the semantic
/// tier embeds for it.
///
/// A unit no declaration describes - a text file chunk - has no entry, so the
/// unit set and the described set are never parallel and never need to be.
/// Pairing at construction is what makes that safe: a caller cannot hand the
/// two over in different orders, and a unit that carries no declaration cannot
/// pick up a vector computed from another unit's text.
#[derive(Clone, Copy, Debug)]
pub struct DescribedUnit<'a> {
    unit: &'a LexicalUnit,
    declaration: Declaration<'a>,
}

impl<'a> DescribedUnit<'a> {
    /// Pairs one indexed unit with the declaration embedded for it.
    #[must_use]
    pub const fn new(unit: &'a LexicalUnit, declaration: Declaration<'a>) -> Self {
        Self { unit, declaration }
    }

    /// The unit this declaration was read from.
    #[must_use]
    pub const fn unit(&self) -> &LexicalUnit {
        self.unit
    }

    /// The declaration whose text is embedded for that unit.
    #[must_use]
    pub const fn declaration(&self) -> &Declaration<'a> {
        &self.declaration
    }
}

/// One unit's document: the text embedded for it, the digest that addresses
/// the vector, and where the unit lives.
#[derive(Clone, Debug)]
struct UnitDocument {
    address: UnitAddress,
    digest: String,
    text: String,
}

/// Which declarations one pass embeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Embedding {
    /// Every declaration the pass was handed, whatever the store holds.
    Every,
    /// Only declarations whose digest the store does not already hold.
    Missing,
}

/// The digest-to-unit map the semantic tier ranks through.
type Addresses = BTreeMap<String, UnitAddress>;

/// Both search tiers over one database file.
///
/// The vector store keys on the digest of the text a declaration was embedded
/// from and the lexical store keys on a unit identity, so neither of them can
/// answer which unit a vector belongs to. The pass that embeds holds that map
/// in memory and publishes it whole, which is why a semantic ranking answers
/// only after a [`SearchIndex::build`] or [`SearchIndex::refresh`] in this
/// process.
#[derive(Debug)]
pub struct SearchIndex {
    lexical: LexicalSearchIndex,
    vectors: SemanticVectorStore,
    model: Mutex<Option<Arc<LoadedModel>>>,
    readiness: Mutex<SemanticReadiness>,
    addresses: Mutex<Arc<Addresses>>,
    limits: SearchIndexLimits,
}

impl SearchIndex {
    /// Opens both stores against one database file.
    ///
    /// The two apply one idempotent migration set, so opening the same file
    /// again reads what the previous open left.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when either store cannot be opened or migrated,
    /// carrying that store's own violation as the cause.
    ///
    /// # Cancel safety
    ///
    /// Cancellation may leave the database file created without its schema
    /// applied. Opening again retries safely: the migrations are idempotent.
    pub async fn open(
        database_path: &Path,
        limits: SearchIndexLimits,
    ) -> Result<Self, SearchError> {
        let lexical = LexicalSearchIndex::open(database_path, limits.lexical())
            .await
            .map_err(store_failed)?;
        let vectors = SemanticVectorStore::open(database_path, limits.lexical())
            .await
            .map_err(store_failed)?;
        Ok(Self {
            lexical,
            vectors,
            model: Mutex::new(None),
            readiness: Mutex::new(limits.initial_readiness()),
            addresses: Mutex::new(Arc::new(Addresses::new())),
            limits,
        })
    }

    /// Loads the encoder, so the semantic tier can answer.
    ///
    /// Runs the acquisition the caller configured, then drops every vector
    /// another model wrote: two models address different spaces, and rows the
    /// previous one left can never be read again.
    ///
    /// A disabled tier acquires nothing and answers `Ok`. A tier already
    /// marked [`SemanticReadiness::Unavailable`] stays that way, so one
    /// failure is final for the life of this index.
    ///
    /// # Errors
    ///
    /// Returns the acquisition's or the encoder's own refusal. Failure marks
    /// the tier `Unavailable` and leaves the lexical tier serving.
    ///
    /// # Cancel safety
    ///
    /// Cancellation leaves the cache as it was found or further along it, and
    /// leaves the readiness the call started with.
    pub async fn prepare(
        &self,
        source: &ModelSource,
        limits: AcquisitionLimits,
    ) -> Result<(), SearchError> {
        if self.readiness() == SemanticReadiness::Disabled {
            return Ok(());
        }
        match self.load(source, limits).await {
            Ok(model) => self.hold(model).await,
            Err(error) => {
                self.set_readiness(SemanticReadiness::Unavailable);
                Err(error)
            }
        }
    }

    /// Replaces the lexical unit set and embeds every declaration handed over.
    ///
    /// This is the pass that establishes a set rather than following one: the
    /// vector it writes for a declaration is the vector this encoder produces
    /// now, whatever the store held before. Use [`SearchIndex::refresh`] for
    /// every later pass, which is the one that trusts what is stored.
    ///
    /// The lexical set is replaced whole here and in every later pass, because
    /// a lexical row costs an insert: reconciling one row against the tree
    /// costs more than writing all of them. A vector costs an embedding pass,
    /// which is why the vector side is the one that reconciles.
    ///
    /// `units` is everything the lexical tier indexes. `described` is the
    /// subset the semantic tier embeds, each entry carrying its own unit, so a
    /// text file chunk that no declaration describes simply has no entry.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when either store refuses, and the encoder's own
    /// refusal when a pass fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before the lexical commit leaves the previous unit set
    /// intact. Cancellation during embedding keeps the vectors already
    /// written, and the next pass embeds what is still missing.
    pub async fn build(
        &self,
        units: &[LexicalUnit],
        described: &[DescribedUnit<'_>],
        tree_revision: &str,
    ) -> Result<(), SearchError> {
        self.pass(units, described, tree_revision, Embedding::Every)
            .await
    }

    /// Same, incrementally: only declarations whose digest is not already
    /// stored are embedded, and vectors no live declaration addresses are
    /// pruned.
    ///
    /// The lexical side is replaced whole and the vector side is not, because
    /// the two cost different things: a lexical row costs an insert, and a
    /// vector costs an embedding pass. A vector is addressed by the digest of
    /// the text it came from, so a declaration that was renamed or moved
    /// without its own bytes changing keeps the vector already stored.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when either store refuses, and the encoder's own
    /// refusal when a pass fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before the lexical commit leaves the previous unit set
    /// intact. Cancellation during embedding keeps the vectors already
    /// written, and the next pass embeds what is still missing.
    pub async fn refresh(
        &self,
        units: &[LexicalUnit],
        described: &[DescribedUnit<'_>],
        tree_revision: &str,
    ) -> Result<(), SearchError> {
        self.pass(units, described, tree_revision, Embedding::Missing)
            .await
    }

    /// Runs both tiers and fuses them.
    ///
    /// The lexical tier always runs. The semantic tier runs when its readiness
    /// says it answers and the store holds vectors this index can address.
    /// Both rankings go to [`fuse`]: a tier that returned nothing contributes
    /// no ranking, and one ranking fused alone is still the fused score, so
    /// two queries' scores mean the same thing.
    ///
    /// An empty query returns nothing. The lexical tier has no term to match,
    /// and the semantic tier would rank the retrieval prefix alone.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when either store refuses, the encoder's own
    /// refusal when the query cannot be embedded, and fusion's refusal when
    /// the configured weights or rank constant cannot carry a score.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; both tiers issue read-only queries.
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<RankedUnit>, SearchError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let lexical = self
            .lexical
            .search(query, limit)
            .await
            .map_err(store_failed)?;
        let semantic = self.semantic(query).await?;
        let fused = self.fused(&lexical, &semantic, limit)?;
        Ok(ranked(&fused, &directory(&lexical, &semantic)))
    }

    /// What the semantic tier can answer right now.
    #[must_use]
    pub fn readiness(&self) -> SemanticReadiness {
        *self
            .readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The tree revision the lexical tier is stamped with.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when the lexical store refuses.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn tree_revision(&self) -> Result<Option<String>, SearchError> {
        self.lexical.tree_revision().await.map_err(store_failed)
    }

    /// Acquires the weights and loads the encoder from them.
    async fn load(
        &self,
        source: &ModelSource,
        limits: AcquisitionLimits,
    ) -> Result<LoadedModel, SearchError> {
        let files = acquire(source, limits).await?;
        let encoder = Encoder::load(&files, self.limits.encoder_limits())?;
        Ok(LoadedModel {
            identity: model_identity(source),
            encoder,
        })
    }

    /// Holds the loaded model and drops every vector another model wrote.
    async fn hold(&self, model: LoadedModel) -> Result<(), SearchError> {
        let _dropped = self
            .vectors
            .prune_other_models(&model.identity)
            .await
            .map_err(store_failed)?;
        let mut held = self.model.lock().unwrap_or_else(PoisonError::into_inner);
        *held = Some(Arc::new(model));
        Ok(())
    }

    /// One pass: the lexical set whole, then the vectors it still needs.
    async fn pass(
        &self,
        units: &[LexicalUnit],
        described: &[DescribedUnit<'_>],
        tree_revision: &str,
        embedding: Embedding,
    ) -> Result<(), SearchError> {
        self.lexical
            .replace_all(units, tree_revision)
            .await
            .map_err(store_failed)?;
        let Some(model) = self.serving_model() else {
            self.note_nothing_embedded(described.len());
            return Ok(());
        };
        self.embed(&model, described, embedding).await
    }

    /// Embeds what this pass owes, prunes what it orphaned, and reports how
    /// far it got.
    async fn embed(
        &self,
        model: &LoadedModel,
        described: &[DescribedUnit<'_>],
        embedding: Embedding,
    ) -> Result<(), SearchError> {
        let total = as_count(described.len());
        let documents = documents(described, as_usize(total.min(self.limits.max_vectors)));
        let stored = self
            .vectors
            .digests(&model.identity)
            .await
            .map_err(store_failed)?;
        self.embed_batches(model, &selected(&documents, &stored, embedding))
            .await?;
        let live: BTreeSet<String> = documents.iter().map(|one| one.digest.clone()).collect();
        let _pruned = self
            .vectors
            .prune_absent(&model.identity, &live)
            .await
            .map_err(store_failed)?;
        self.publish(&documents);
        self.set_readiness(reached(as_count(documents.len()), total));
        Ok(())
    }

    /// Embeds `wanted` in passes of `batch_declarations`, storing each pass
    /// before the next runs.
    ///
    /// At most `max_vectors / batch_declarations` passes run: `wanted` was cut
    /// to `max_vectors` before the pairing that produced it. Storing per pass
    /// is what makes a cancelled build keep the work it already paid for.
    async fn embed_batches(
        &self,
        model: &LoadedModel,
        wanted: &[&UnitDocument],
    ) -> Result<(), SearchError> {
        for chunk in wanted.chunks(batch_size(self.limits.batch_declarations)) {
            let texts: Vec<String> = chunk.iter().map(|one| one.text.clone()).collect();
            let embedded = model.encoder.embed_documents(&texts)?;
            let vectors = paired(chunk, embedded);
            self.vectors
                .store(&model.identity, model.encoder.dimension(), &vectors)
                .await
                .map_err(store_failed)?;
        }
        Ok(())
    }

    /// The semantic ranking, or nothing when the tier cannot answer.
    ///
    /// A digest the address map does not name is skipped: the store holds
    /// vectors from before this process opened it, and until a pass publishes
    /// the map there is no unit to rank them as.
    async fn semantic(&self, query: &str) -> Result<Vec<UnitAddress>, SearchError> {
        let Some(model) = self.serving_model() else {
            return Ok(Vec::new());
        };
        let stored = self
            .vectors
            .vectors(&model.identity, model.encoder.dimension())
            .await
            .map_err(store_failed)?;
        if stored.is_empty() {
            return Ok(Vec::new());
        }
        let addresses = self.addresses();
        let embedded = model.encoder.embed_query(query)?;
        let matched = nearest(&embedded, &stored, self.limits.depth())?;
        let placed = placed(&matched, &addresses);
        let spread = spread_per_file(&placed, as_usize(self.limits.per_file_max));
        Ok(resolved(
            &spread,
            &addresses,
            as_usize(self.limits.candidates),
        ))
    }

    /// The two rankings fused, kept to what the caller asked for.
    fn fused(
        &self,
        lexical: &[LexicalMatch],
        semantic: &[UnitAddress],
        limit: u32,
    ) -> Result<Vec<FusedRank>, SearchError> {
        let lexical_order: Vec<&str> = lexical.iter().map(LexicalMatch::identity).collect();
        let semantic_order: Vec<&str> = semantic
            .iter()
            .map(|address| address.identity.as_str())
            .collect();
        let rankings = [
            Ranking::new(self.limits.lexical_weight, &lexical_order),
            Ranking::new(self.limits.semantic_weight, &semantic_order),
        ];
        fuse(&rankings, self.limits.fusion_k, as_usize(u64::from(limit)))
    }

    /// The model this index ranks through, when its readiness lets it answer.
    fn serving_model(&self) -> Option<Arc<LoadedModel>> {
        if !self.readiness().answers() {
            return None;
        }
        let held = self.model.lock().unwrap_or_else(PoisonError::into_inner);
        held.as_ref().map(Arc::clone)
    }

    /// The published digest-to-unit map.
    fn addresses(&self) -> Arc<Addresses> {
        let held = self
            .addresses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&held)
    }

    /// Publishes the map this pass built, replacing the previous one whole.
    fn publish(&self, documents: &[UnitDocument]) {
        let addresses: Addresses = documents
            .iter()
            .map(|one| (one.digest.clone(), one.address.clone()))
            .collect();
        let mut held = self
            .addresses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *held = Arc::new(addresses);
    }

    /// Records that a pass embedded nothing, without clearing a tier that is
    /// off or one that already failed for good.
    fn note_nothing_embedded(&self, total: usize) {
        if matches!(self.readiness(), SemanticReadiness::Preparing { .. }) {
            self.set_readiness(SemanticReadiness::Preparing {
                prepared: 0,
                total: as_count(total),
            });
        }
    }

    /// Records how far the semantic tier has got.
    fn set_readiness(&self, readiness: SemanticReadiness) {
        let mut held = self
            .readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *held = readiness;
    }
}

/// The documents this pass embeds, each under the unit it was described for.
///
/// Every entry carries its own unit, so the digest a document is stored under
/// and the unit a ranking resolves it to come from one [`DescribedUnit`] and
/// cannot be paired by position. The cut at `bound`, which is `max_vectors`
/// capped by the described count, is where the workspace's vector ceiling is
/// applied; every loop below this one runs over what it returns.
fn documents(described: &[DescribedUnit<'_>], bound: usize) -> Vec<UnitDocument> {
    let kept = &described[..bound.min(described.len())];
    let texts: Vec<String> = kept
        .iter()
        .map(|one| document(one.declaration()).into_text())
        .collect();
    let keys = digests(&texts);
    kept.iter()
        .zip(texts)
        .zip(keys)
        .map(|((one, text), key)| UnitDocument {
            address: UnitAddress::of(one.unit()),
            digest: key.to_hex(),
            text,
        })
        .collect()
}

/// The documents this pass owes a vector, each digest once.
///
/// One text has one digest, so two declarations that read alike are one
/// embedding. The loop runs over a slice already cut to `max_vectors`.
fn selected<'a>(
    documents: &'a [UnitDocument],
    stored: &BTreeSet<String>,
    embedding: Embedding,
) -> Vec<&'a UnitDocument> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut wanted: Vec<&UnitDocument> = Vec::with_capacity(documents.len());
    for one in documents {
        let held = embedding == Embedding::Missing && stored.contains(&one.digest);
        if !held && seen.insert(&one.digest) {
            wanted.push(one);
        }
    }
    wanted
}

/// One pass's vectors under the digests they were embedded from.
fn paired(chunk: &[&UnitDocument], embedded: Vec<Vec<f32>>) -> Vec<StoredVector> {
    chunk
        .iter()
        .zip(embedded)
        .map(|(one, values)| StoredVector::new(one.digest.clone(), values))
        .collect()
}

/// Each semantic match under the file its declaration lives in.
///
/// The loop runs over what `nearest` returned, which is already the caller's
/// own depth bound.
fn placed(matched: &[SemanticMatch], addresses: &Addresses) -> Vec<DeclarationMatch> {
    matched
        .iter()
        .filter_map(|one| {
            addresses
                .get(one.digest())
                .map(|address| DeclarationMatch::new(address.path.clone(), one.clone()))
        })
        .collect()
}

/// The units a spread semantic ranking names, best first, keeping `keep_max`.
fn resolved(
    spread: &[DeclarationMatch],
    addresses: &Addresses,
    keep_max: usize,
) -> Vec<UnitAddress> {
    spread
        .iter()
        .take(keep_max)
        .filter_map(|one| addresses.get(one.matched().digest()).cloned())
        .collect()
}

/// Where every unit either ranking named lives.
///
/// A fused identity always came from one of the two rankings, so this map
/// names every identity fusion can return.
fn directory<'a>(
    lexical: &'a [LexicalMatch],
    semantic: &'a [UnitAddress],
) -> BTreeMap<&'a str, (&'a ProjectPath, LexicalUnitKind)> {
    let from_lexical = lexical
        .iter()
        .map(|one| (one.identity(), (one.path(), one.kind())));
    let from_semantic = semantic
        .iter()
        .map(|one| (one.identity.as_str(), (&one.path, one.kind)));
    from_lexical.chain(from_semantic).collect()
}

/// The fused ranks as units, in the order fusion produced.
fn ranked(
    fused: &[FusedRank],
    directory: &BTreeMap<&str, (&ProjectPath, LexicalUnitKind)>,
) -> Vec<RankedUnit> {
    fused
        .iter()
        .filter_map(|rank| {
            directory
                .get(rank.identity())
                .map(|(path, kind)| RankedUnit {
                    identity: rank.identity().to_owned(),
                    path: (*path).clone(),
                    kind: *kind,
                    score: rank.score(),
                })
        })
        .collect()
}

/// The readiness a pass that embedded `prepared` of `total` reaches.
const fn reached(prepared: u64, total: u64) -> SemanticReadiness {
    if prepared == total {
        SemanticReadiness::Ready
    } else {
        SemanticReadiness::Preparing { prepared, total }
    }
}

/// The identity one model's vectors are addressed under.
///
/// A repository carries its revision: two revisions are two checkpoints, and
/// their vectors share no space. A directory carries the path it was read
/// from.
fn model_identity(source: &ModelSource) -> String {
    match source {
        ModelSource::Repository {
            repository,
            revision,
        } => format!("{repository}@{revision}"),
        ModelSource::Directory(directory) => directory.display().to_string(),
    }
}

/// One store failure, with that store's own violation riding as the cause.
fn store_failed(source: LexicalIndexError) -> SearchError {
    let subject = source.to_string();
    SearchError::new(
        SearchFault::new(SearchViolation::StoreFailed)
            .about(subject)
            .caused_by(source),
    )
}

/// Declarations one pass takes, never zero: a pass of nothing divides the
/// work into no passes at all and embeds nothing.
fn batch_size(batch_declarations: u64) -> usize {
    let batch = as_usize(batch_declarations);
    if batch == 0 { 1 } else { batch }
}

/// One bound as the in-memory APIs take it.
fn as_usize(bound: u64) -> usize {
    usize::try_from(bound).unwrap_or(usize::MAX)
}

/// One count as the readiness reports it.
fn as_count(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        Embedding, LEXICAL_WEIGHT_DEFAULT, SearchIndexLimits, SemanticReadiness, UnitAddress,
        UnitDocument, as_count, as_usize, batch_size, model_identity, reached, selected,
    };
    use crate::acquisition::ModelSource;
    use rift_core::ProjectPath;
    use rift_index::{LexicalIndexLimits, LexicalUnitKind};
    use std::collections::BTreeSet;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn documented(digest: &str) -> Result<UnitDocument, Box<dyn std::error::Error>> {
        Ok(UnitDocument {
            address: UnitAddress {
                identity: digest.to_owned(),
                path: ProjectPath::new("src/lib.rs".to_owned())?,
                kind: LexicalUnitKind::Symbol,
            },
            digest: digest.to_owned(),
            text: format!("fn {digest}"),
        })
    }

    #[test]
    fn test_a_pass_that_covered_every_declaration_is_ready() {
        assert_eq!(reached(0, 0), SemanticReadiness::Ready);
        assert_eq!(reached(4, 4), SemanticReadiness::Ready);
        assert_eq!(
            reached(2, 5),
            SemanticReadiness::Preparing {
                prepared: 2,
                total: 5
            }
        );
    }

    #[test]
    fn test_readiness_answers_while_it_is_ready_or_preparing() {
        assert!(SemanticReadiness::Ready.answers());
        assert!(
            SemanticReadiness::Preparing {
                prepared: 1,
                total: 2
            }
            .answers()
        );
        assert!(!SemanticReadiness::Disabled.answers());
        assert!(!SemanticReadiness::Unavailable.answers());
    }

    #[test]
    fn test_a_missing_pass_skips_what_is_stored_and_a_full_pass_does_not() -> TestResult {
        let documents = [documented("aaa")?, documented("bbb")?];
        let stored: BTreeSet<String> = ["aaa".to_owned()].into_iter().collect();
        let missing = selected(&documents, &stored, Embedding::Missing);
        assert_eq!(
            missing
                .iter()
                .map(|one| one.digest.as_str())
                .collect::<Vec<_>>(),
            ["bbb"]
        );
        let every = selected(&documents, &stored, Embedding::Every);
        assert_eq!(every.len(), 2, "a full pass embeds what is stored again");
        Ok(())
    }

    #[test]
    fn test_one_digest_is_embedded_once_however_often_it_repeats() -> TestResult {
        let documents = [documented("aaa")?, documented("aaa")?];
        let selected = selected(&documents, &BTreeSet::new(), Embedding::Every);
        assert_eq!(selected.len(), 1);
        Ok(())
    }

    #[test]
    fn test_a_batch_of_zero_falls_to_one_declaration_per_pass() {
        assert_eq!(batch_size(0), 1);
        assert_eq!(batch_size(32), 32);
        assert_eq!(as_usize(u64::MAX), usize::MAX);
        assert_eq!(as_count(7), 7);
    }

    #[test]
    fn test_a_model_identity_carries_the_revision_or_the_directory() -> TestResult {
        let repository = ModelSource::repository("BAAI/bge-small-en-v1.5")?;
        assert_eq!(model_identity(&repository), "BAAI/bge-small-en-v1.5@main");
        let pinned = ModelSource::repository("BAAI/bge-small-en-v1.5@dd0a482")?;
        assert_eq!(
            model_identity(&pinned),
            "BAAI/bge-small-en-v1.5@dd0a482",
            "a pinned revision addresses its own space"
        );
        let directory = ModelSource::Directory(std::path::PathBuf::from("models/bge"));
        assert!(model_identity(&directory).contains("models/bge"));
        Ok(())
    }

    #[test]
    fn test_the_builder_carries_every_bound_it_was_given() {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default())
            .weights(0.5, 0.5)
            .fusion_k(7)
            .candidates(9)
            .max_vectors(11)
            .batch_declarations(13)
            .max_tokens(64)
            .per_file_max(2)
            .disable_semantic()
            .build();
        assert!((limits.lexical_weight() - 0.5).abs() < f64::EPSILON);
        assert!((limits.semantic_weight() - 0.5).abs() < f64::EPSILON);
        assert_eq!(limits.fusion_k(), 7);
        assert_eq!(limits.candidates(), 9);
        assert_eq!(limits.max_vectors(), 11);
        assert_eq!(limits.batch_declarations(), 13);
        assert_eq!(limits.max_tokens(), 64);
        assert_eq!(limits.per_file_max(), 2);
        assert!(limits.is_semantic_disabled());
        assert_eq!(limits.lexical(), LexicalIndexLimits::default());
        assert_eq!(limits.initial_readiness(), SemanticReadiness::Disabled);
        assert_eq!(limits.depth(), 18);
        assert_eq!(limits.encoder_limits().tokens_max(), 64);
        assert_eq!(limits.encoder_limits().batch_declarations(), 13);
        assert_eq!(limits.encoder_limits().texts_max(), 13);
    }

    #[test]
    fn test_the_shipped_bounds_start_preparing_with_nothing_embedded() {
        let limits = SearchIndexLimits::default();
        assert!((limits.lexical_weight() - LEXICAL_WEIGHT_DEFAULT).abs() < f64::EPSILON);
        assert!(!limits.is_semantic_disabled());
        assert_eq!(
            limits.initial_readiness(),
            SemanticReadiness::Preparing {
                prepared: 0,
                total: 0
            }
        );
        assert_eq!(limits.depth(), 600);
    }
}
