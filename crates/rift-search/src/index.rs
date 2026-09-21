//! One index over both search tiers, and how far the vector ranking has got.
//!
//! [`SearchIndex`] owns the lexical index and the vector store against one
//! database file, so a caller drives search through it and never opens either
//! store itself. One `search` runs both tiers and fuses what they returned.
//!
//! Nothing here knows what a server is. The caller drives
//! [`SearchIndex::prepare`], [`SearchIndex::replace_lexical`], and
//! [`SearchIndex::embed_described`] on whatever task it likes. The encoder's
//! forward pass does not run on the calling task: candle would hold a runtime
//! worker for the length of a batch, so every call into the encoder goes
//! through `tokio::task::spawn_blocking` and the runtime's workers stay free
//! for whatever else the caller is serving.
//!
//! What the vector ranking can answer right now is [`VectorReadiness`]. This
//! crate reports it and stops there: the wire warning a caller attaches to a
//! result is built above, by the layer that owns the protocol models, because
//! the search tier sits below that layer and never depends on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rift_core::ProjectPath;
use rift_index::{DatabasePool, WorkspaceDatabase};
use rift_index::{
    LexicalChange, LexicalIndexError, LexicalIndexLimits, LexicalSearchIndex, RevisionScoped,
    StoredVector, VectorStore,
};
use rift_ranking::{
    DocumentIdentity, DocumentLocation, FieldSet, IndexDocument, ParsedQuery, QueryPhase,
    RankedIdentity, RankingInput, RankingInputKind, SearchableField,
};

use crate::acquisition::{AcquisitionLimits, ModelSource, acquire};
use crate::document::{Declaration, digests, document};
use crate::embedding::{
    BatchSchedule, EmbeddingModels, EmbeddingSpace, LocalEncoder, RetrievalModels,
};
use crate::encoder::{Encoder, EncoderLimits};
use crate::error::{SearchError, SearchFault, SearchViolation};
use crate::fusion::{DeclarationMatch, spread_per_file};
use crate::similarity::{VectorMatch, nearest};

/// Embedding requests open at once when the caller sets none. A locally run
/// encoder holds a blocking thread, so it runs one whatever this says.
const MAX_IN_FLIGHT_DEFAULT: u64 = 1;
/// Declarations the vector ranking returns when the caller sets none.
const CANDIDATES_DEFAULT: u64 = 200;
/// Vectors the workspace may hold when the caller sets none.
const MAX_VECTORS_DEFAULT: u64 = 200_000;
/// Declarations one embedding pass takes when the caller sets none.
const BATCH_DECLARATIONS_DEFAULT: u64 = 32;
/// Tokens the encoder reads from one declaration when the caller sets none.
const MAX_TOKENS_DEFAULT: u64 = 256;
/// Candidates one file may contribute when the caller sets none.
const PER_FILE_MAX_DEFAULT: u64 = 3;

/// How far the vector ranking has got.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorReadiness {
    /// No vector ranking: the workspace turned it off.
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

impl VectorReadiness {
    /// Whether the tier may take part in a ranking.
    ///
    /// `Preparing` counts: what is already embedded ranks, and a partial
    /// ranking beside the lexical one is worth more than none.
    const fn answers(self) -> bool {
        matches!(self, Self::Ready | Self::Preparing { .. })
    }
}

/// What one store read answered for one query phase.
///
///
/// The store contributes ordered identities, never a fused score: the caller
/// fuses them with the identifier ranking it built itself, so all three
/// inputs meet in one place under one set of weights.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoreRanking {
    inputs: Vec<RankingInput>,
    lexical_truncated_at: Option<u32>,
}

impl StoreRanking {
    /// Names the inputs one phase produced and the bound the full-text
    /// ranking stopped at.
    #[must_use]
    pub const fn new(inputs: Vec<RankingInput>, lexical_truncated_at: Option<u32>) -> Self {
        Self {
            inputs,
            lexical_truncated_at,
        }
    }

    /// The inputs, in the order the store ran them.
    #[must_use]
    pub fn inputs(&self) -> &[RankingInput] {
        &self.inputs
    }

    /// The inputs, owned.
    #[must_use]
    pub fn into_inputs(self) -> Vec<RankingInput> {
        self.inputs
    }

    /// The bound the full-text ranking stopped at while its store held a
    /// match past it, or `None` when it ranked every match.
    #[must_use]
    pub const fn lexical_truncated_at(&self) -> Option<u32> {
        self.lexical_truncated_at
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
    candidates: u64,
    max_vectors: u64,
    batch_declarations: u64,
    max_in_flight: u64,
    max_tokens: u64,
    per_file_max: u64,
    vector_disabled: bool,
}

impl SearchIndexLimits {
    /// Starts from the lexical bounds both stores open under.
    pub const fn builder(lexical: LexicalIndexLimits) -> SearchIndexLimitsBuilder {
        SearchIndexLimitsBuilder {
            limits: Self {
                lexical,
                candidates: CANDIDATES_DEFAULT,
                max_vectors: MAX_VECTORS_DEFAULT,
                batch_declarations: BATCH_DECLARATIONS_DEFAULT,
                max_in_flight: MAX_IN_FLIGHT_DEFAULT,
                max_tokens: MAX_TOKENS_DEFAULT,
                per_file_max: PER_FILE_MAX_DEFAULT,
                vector_disabled: false,
            },
        }
    }

    /// The bounds both stores open under.
    #[must_use]
    pub const fn lexical(self) -> LexicalIndexLimits {
        self.lexical
    }

    /// Declarations the vector ranking returns before the two are fused.
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

    /// Embedding requests that may be open at once. A locally run encoder
    /// runs one whatever this says.
    #[must_use]
    pub const fn max_in_flight(self) -> u64 {
        self.max_in_flight
    }

    /// Tokens the encoder reads from one declaration.
    #[must_use]
    pub const fn max_tokens(self) -> u64 {
        self.max_tokens
    }

    /// Candidates one file may contribute to the vector ranking.
    #[must_use]
    pub const fn per_file_max(self) -> u64 {
        self.per_file_max
    }

    /// Whether the workspace turned the vector ranking off.
    #[must_use]
    pub const fn is_vector_disabled(self) -> bool {
        self.vector_disabled
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
    const fn initial_readiness(self) -> VectorReadiness {
        if self.vector_disabled {
            VectorReadiness::Disabled
        } else {
            VectorReadiness::Preparing {
                prepared: 0,
                total: 0,
            }
        }
    }

    /// How deep the vector ranking is read before it is spread across files.
    ///
    /// A file may keep `per_file_max` candidates, so reading the ranking that
    /// many times deeper than the candidate list leaves a full list even when
    /// one file's declarations hold the whole head.
    fn depth(self) -> usize {
        as_usize(self.candidates.saturating_mul(self.per_file_max))
    }
}

impl Default for SearchIndexLimits {
    /// The shipped bounds: the lexical defaults, 200 vector candidates spread
    /// 3 per file, and 200,000 vectors embedded 32 at a time over 256 tokens
    /// each. The shares each ranking carries live with fusion, which this
    /// crate no longer runs.
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
    /// Sets how many declarations the vector ranking returns.
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

    /// Sets how many embedding requests may be open at once.
    pub const fn max_in_flight(mut self, max_in_flight: u64) -> Self {
        self.limits.max_in_flight = max_in_flight;
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

    /// Turns the vector ranking off, so nothing is acquired and nothing is
    /// embedded.
    pub const fn disable_vector(mut self) -> Self {
        self.limits.vector_disabled = true;
        self
    }

    /// The finished bounds.
    ///
    /// Nothing is validated here. Acceptance already refused a value outside
    /// its range, and a second check would be a second representation of the
    /// same rule.
    #[must_use]
    pub const fn build(self) -> SearchIndexLimits {
        self.limits
    }
}

/// The loaded encoder and the identity its vectors are addressed under.
#[derive(Debug)]
struct LoadedModel {
    /// The space this model embeds into. Its identity is what the stored
    /// vectors are filed under, so a changed endpoint, model, revision,
    /// width, or transformation drops them rather than scoring a query
    /// against coordinates from another space.
    space: EmbeddingSpace,
    /// The document and query handles Rig's `EmbeddingModel` contract is
    /// called through.
    models: EmbeddingModels,
}

/// Where one ranked document lives, without the content either store holds.
///
/// The path is here for the per-file spread bound alone: it decides how many
/// of one file's declarations may reach the candidate list. Nothing past that
/// bound reads it, and no path leaves this crate in a ranking.
#[derive(Clone, Debug)]
struct UnitAddress {
    identity: DocumentIdentity,
    path: ProjectPath,
}

impl UnitAddress {
    /// The address of one indexed document, or `None` for a package document:
    /// the vector lane runs over the project store alone.
    fn of(document: &IndexDocument) -> Option<Self> {
        match document.location() {
            DocumentLocation::Project(path) => Some(Self {
                identity: document.identity().clone(),
                path: path.clone(),
            }),
            DocumentLocation::Unit(_) => None,
        }
    }
}

/// One index document together with the declaration whose text the vector
/// ranking embeds for it.
///
/// A document no declaration describes - a text file chunk - has no entry, so
/// the document set and the described set are never parallel and never need
/// to be. Pairing at construction is what makes that safe: a caller cannot
/// hand the two over in different orders, and a document that carries no
/// declaration cannot pick up a vector computed from another one's text.
#[derive(Clone, Copy, Debug)]
pub struct DescribedUnit<'a> {
    unit: &'a IndexDocument,
    declaration: Declaration<'a>,
}

impl<'a> DescribedUnit<'a> {
    /// Pairs one indexed document with the declaration embedded for it.
    #[must_use]
    pub const fn new(unit: &'a IndexDocument, declaration: Declaration<'a>) -> Self {
        Self { unit, declaration }
    }

    /// The document this declaration was read from.
    #[must_use]
    pub const fn unit(&self) -> &IndexDocument {
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
pub enum Embedding {
    /// Every declaration the pass was handed, whatever the store holds.
    Every,
    /// Only declarations whose digest the store does not already hold.
    Missing,
}

/// The digest-to-unit map the vector ranking ranks through.
type Addresses = BTreeMap<String, UnitAddress>;

/// The vectors the vector ranking scans, as one pass read them back.
type Corpus = Vec<StoredVector>;

/// What one embedding pass published: the vectors it read back, the map that places each
/// of them on a unit, and the tree revision the declarations behind them were described
/// for.
///
/// The three are held as one value because a query needs all three to agree. Vectors read
/// against another pass's map place a digest on the wrong unit, and either read against a
/// tree they were not described for ranks declarations the published workspace no longer
/// holds.
#[derive(Debug)]
struct HeldCorpus {
    tree_revision: String,
    addresses: Addresses,
    vectors: Corpus,
}

/// Both search tiers over one database file.
///
/// The vector store keys on the digest of the text a declaration was embedded
/// from and the lexical store keys on a unit identity, so neither of them can
/// answer which unit a vector belongs to. The pass that embeds holds that map
/// in memory and publishes it whole, which is why a vector ranking answers
/// only after a [`SearchIndex::embed_described`] in this process.
///
/// The vectors themselves are held beside that map, and the pass publishes the
/// two together. Reading them back per query instead cost one `SELECT` over
/// every row this workspace embedded and one decode of every blob it returned,
/// paid before a single row was scored and paid again for the next query. What
/// holding them costs is the ceiling the operator already set:
/// `search.vector.max_vectors` vectors, each as wide as the encoder's
/// dimension, held for as long as this index is open. The store stays the
/// record: a restart reads the corpus back once, in the first pass, rather
/// than embedding it again.
#[derive(Debug)]
pub struct SearchIndex {
    lexical: LexicalSearchIndex,
    vectors: VectorStore,
    model: Mutex<Option<Arc<LoadedModel>>>,
    readiness: Mutex<VectorReadiness>,
    held: Mutex<Option<Arc<HeldCorpus>>>,
    limits: SearchIndexLimits,
}

impl SearchIndex {
    /// Opens the workspace database and attaches both tiers to its one pool.
    ///
    /// The tiers share the pool rather than opening one each: `SQLite`
    /// serializes writers per file, so a second pool adds connections that lose
    /// the same lock, never throughput.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when the database cannot be opened or migrated.
    ///
    /// # Cancel safety
    ///
    /// Cancellation may leave the database file created without its schema
    /// applied. Opening again retries safely: the migrations are idempotent.
    pub async fn open(
        database_path: &Path,
        limits: SearchIndexLimits,
    ) -> Result<Self, SearchError> {
        let lexical_limits = limits.lexical();
        let pool = DatabasePool::new(
            lexical_limits.pool_slots(),
            lexical_limits.busy_timeout_ms(),
        );
        let database = WorkspaceDatabase::open(database_path, pool)
            .await
            .map_err(store_failed)?;
        Self::attached(database, limits)
    }

    /// Attaches both tiers to one already-open workspace database.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when a tier refuses the database.
    pub fn attached(
        database: Arc<WorkspaceDatabase>,
        limits: SearchIndexLimits,
    ) -> Result<Self, SearchError> {
        let lexical = LexicalSearchIndex::attached(Arc::clone(&database), limits.lexical());
        let vectors = VectorStore::attached(database);
        Ok(Self {
            lexical,
            vectors,
            model: Mutex::new(None),
            readiness: Mutex::new(limits.initial_readiness()),
            held: Mutex::new(None),
            limits,
        })
    }

    /// Loads the encoder, so the vector ranking can answer.
    ///
    /// Runs the acquisition the caller configured, then drops every vector
    /// another model wrote and clears the held corpus with them: two models
    /// address different spaces, and rows the previous one left can never be
    /// read again.
    ///
    /// A disabled tier acquires nothing and answers `Ok`. A tier already
    /// marked [`VectorReadiness::Unavailable`] stays that way, so one
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
        if self.readiness() == VectorReadiness::Disabled {
            return Ok(());
        }
        match self.load(source, limits).await {
            Ok(model) => self.hold(model).await,
            Err(error) => {
                self.set_readiness(VectorReadiness::Unavailable);
                Err(error)
            }
        }
    }

    /// Holds a model pair the caller already built, so the vector ranking can
    /// answer through it.
    ///
    /// A service answering the embedding shape needs no acquisition: the
    /// configuration states its endpoint, its model, and the width every
    /// vector carries, and the client is built from those. Vectors another
    /// space wrote are dropped here exactly as they are after an acquisition,
    /// because two spaces are two coordinate systems.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when the vector store refuses to drop the
    /// previous space's rows.
    pub async fn hold_models(
        &self,
        models: EmbeddingModels,
        space: EmbeddingSpace,
    ) -> Result<(), SearchError> {
        if self.readiness() == VectorReadiness::Disabled {
            return Ok(());
        }
        self.hold(LoadedModel { space, models }).await
    }

    /// The bounds the lexical tier enforces on what it is handed.
    #[must_use]
    pub const fn lexical_limits(&self) -> LexicalIndexLimits {
        self.limits.lexical()
    }

    /// Replaces the lexical unit set and embeds every declaration handed over.
    ///
    /// This is the pass that establishes a set rather than following one: the
    /// vector it writes for a declaration is the vector this encoder produces
    /// Replaces the whole lexical set and stamps `tree_revision`, in one transaction.
    ///
    /// Startup and every rebuild that reads the whole workspace take this path: a set the
    /// index cannot name the difference against is cheaper to write whole than to
    /// reconcile row by row.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when the lexical store refuses.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before the commit leaves the previous unit set and stamp intact.
    pub async fn replace_lexical(
        &self,
        documents: &[IndexDocument],
        tree_revision: &str,
    ) -> Result<(), SearchError> {
        self.lexical
            .replace_all(documents, tree_revision)
            .await
            .map_err(store_failed)
    }

    /// Applies one change set's lexical units and stamps `tree_revision`, in one
    /// transaction.
    ///
    /// A rebuild that named the files it read pays one delete and one insert batch per
    /// changed path, against a rewrite of every indexed unit.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when the lexical store refuses.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before the commit leaves the previous unit set and stamp intact.
    pub async fn apply_lexical(
        &self,
        change: &LexicalChange,
        tree_revision: &str,
    ) -> Result<(), SearchError> {
        self.lexical
            .apply(change, tree_revision)
            .await
            .map_err(store_failed)
    }

    /// Embeds the declarations one publication describes, prunes the vectors no live
    /// declaration addresses, and publishes the corpus the vector ranking scans.
    ///
    /// This is the half a request never waits for. A vector costs an embedding pass, which
    /// can run for longer than any freshness deadline, while a lexical row costs an insert.
    ///
    /// `Embedding::Every` establishes the vector set, which the first pass of a run does
    /// because a store found on disk was written by an earlier process, possibly under
    /// another model. `Embedding::Missing` trusts what is stored and embeds only what a
    /// declaration's own bytes changed.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when the vector store refuses, and the encoder's own refusal
    /// when a pass fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation keeps the vectors already written, and the next pass embeds what is
    /// still missing.
    pub async fn embed_described(
        &self,
        described: &[DescribedUnit<'_>],
        embedding: Embedding,
        tree_revision: &str,
    ) -> Result<(), SearchError> {
        let Some(model) = self.serving_model() else {
            self.note_nothing_embedded(described.len());
            return Ok(());
        };
        self.embed(&model, described, embedding, tree_revision)
            .await
    }

    /// Runs the store's ranking inputs for one query phase, best first.
    ///
    /// The full-text ranking always runs. The vector ranking runs in the
    /// precise phase alone, and only when its readiness says it answers and a
    /// pass has published a corpus to scan: an embedding reads the whole
    /// question, so widening the unquoted terms produces the same vector
    /// order the precise phase already contributed, and the caller's own
    /// deduplication would drop every one of them.
    ///
    /// Neither input carries a score across this boundary. The caller fuses
    /// the two returned here with the identifier ranking it built itself, so
    /// all three meet under one set of weights.
    ///
    /// A query carrying no member returns no input: the full-text ranking has
    /// no term to match, and the vector ranking would rank the retrieval
    /// prefix alone.
    ///
    /// # Errors
    ///
    /// Returns `store_failed` when either store refuses, and the encoder's
    /// own refusal when the query cannot be embedded.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; both stores issue read-only queries.
    pub async fn rank(
        &self,
        tree_revision: &str,
        query: &ParsedQuery,
        phase: QueryPhase,
        limit: u32,
    ) -> Result<RevisionScoped<StoreRanking>, SearchError> {
        let lexical = match self
            .lexical
            .search(tree_revision, query, phase, limit)
            .await
            .map_err(store_failed)?
        {
            RevisionScoped::Matched(ranking) => ranking,
            RevisionScoped::OtherRevision(stored) => {
                return Ok(RevisionScoped::OtherRevision(stored));
            }
            RevisionScoped::NoRevision => return Ok(RevisionScoped::NoRevision),
        };
        if query.is_empty() {
            return Ok(RevisionScoped::Matched(StoreRanking::default()));
        }
        let lexical_truncated_at = lexical.truncated_at();
        let mut inputs = vec![lexical.into_input()];
        if phase == QueryPhase::Precise {
            inputs.push(self.vector_input(query.source(), tree_revision).await?);
        }
        Ok(RevisionScoped::Matched(StoreRanking {
            inputs,
            lexical_truncated_at,
        }))
    }

    /// What the vector ranking can answer right now.
    #[must_use]
    pub fn readiness(&self) -> VectorReadiness {
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
        let space = local_embedding_space(source, encoder.dimension());
        let held = LocalEncoder::new(Arc::new(encoder));
        Ok(LoadedModel {
            models: EmbeddingModels::Local(RetrievalModels::new(
                held.documents(),
                held.query(),
                space.clone(),
            )),
            space,
        })
    }

    /// Holds the loaded model, drops every vector another model wrote, and
    /// empties the held corpus.
    ///
    /// The corpus goes because whatever is in it was embedded by the model
    /// this index held before: scoring a query this encoder embedded against
    /// another encoder's vectors ranks two spaces against each other. The
    /// emptying lands before the new model does, so no query can read this
    /// model beside the previous one's vectors; the next pass reads the corpus
    /// back under the model held here.
    async fn hold(&self, model: LoadedModel) -> Result<(), SearchError> {
        let _dropped = self
            .vectors
            .prune_other_models(&model.space.identity())
            .await
            .map_err(store_failed)?;
        self.publish_held(None);
        let mut held = self.model.lock().unwrap_or_else(PoisonError::into_inner);
        *held = Some(Arc::new(model));
        Ok(())
    }

    /// Embeds what this pass owes, prunes what it orphaned, reads the corpus
    /// back, and reports how far it got.
    ///
    /// The corpus is read from the store rather than kept from what this pass
    /// embedded, because a refresh embeds only what was missing: everything a
    /// previous pass wrote is a vector this one never saw. One read per pass
    /// is what the query path no longer pays per query.
    ///
    /// # Cancel safety
    ///
    /// A pass publishes the address map and the corpus once, after it has
    /// embedded and pruned, so a dropped future leaves the previous pair
    /// serving rather than a half-built one.
    async fn embed(
        &self,
        model: &Arc<LoadedModel>,
        described: &[DescribedUnit<'_>],
        embedding: Embedding,
        tree_revision: &str,
    ) -> Result<(), SearchError> {
        let total = as_count(described.len());
        let documents = documents(described, as_usize(total.min(self.limits.max_vectors)));
        let stored = self
            .vectors
            .digests(&model.space.identity())
            .await
            .map_err(store_failed)?;
        self.embed_batches(model, &selected(&documents, &stored, embedding))
            .await?;
        let live: BTreeSet<String> = documents.iter().map(|one| one.digest.clone()).collect();
        let _pruned = self
            .vectors
            .prune_absent(&model.space.identity(), &live)
            .await
            .map_err(store_failed)?;
        let corpus = self.read_corpus(model).await?;
        self.publish(&documents, corpus, tree_revision);
        self.set_readiness(reached(as_count(documents.len()), total));
        Ok(())
    }

    /// The vectors this model holds, read back for the vector ranking to scan.
    ///
    /// The read stops at `max_vectors`, the same ceiling `documents` cuts the
    /// described set to, because this is what the index goes on holding: a
    /// corpus is `max_vectors` vectors of the encoder's dimension in memory,
    /// and an operator who raises the key raises that. The prune this pass
    /// just ran already left the store at that ceiling, so the cut answers
    /// only a store another index wrote to under a wider one: it drops the
    /// tail of the digest order rather than refusing the pass.
    async fn read_corpus(&self, model: &LoadedModel) -> Result<Corpus, SearchError> {
        self.vectors
            .vectors(
                &model.space.identity(),
                model.space.dimensions(),
                as_usize(self.limits.max_vectors),
            )
            .await
            .map_err(store_failed)
    }

    /// Embeds `wanted` in passes of `batch_declarations`, storing each pass
    /// before the next runs.
    ///
    /// At most `max_vectors / batch_declarations` passes run: `wanted` was cut
    /// to `max_vectors` before the pairing that produced it. Storing per pass
    /// is what makes a cancelled build keep the work it already paid for.
    ///
    /// Every batch goes through Rig's `EmbeddingModel` contract, whichever
    /// family serves it. A local encoder runs on a blocking thread, because
    /// candle's forward pass would otherwise hold a runtime worker for the
    /// length of the batch; a remote endpoint runs at most `max_in_flight`
    /// requests at once. Dropping this future does not cancel a batch already
    /// handed over: it finishes unread, its vectors are never stored, and the
    /// next pass embeds it again.
    async fn embed_batches(
        &self,
        model: &Arc<LoadedModel>,
        wanted: &[&UnitDocument],
    ) -> Result<(), SearchError> {
        let batch = batch_size(self.limits.batch_declarations);
        let schedule = BatchSchedule::new(
            batch,
            model
                .models
                .requests_in_flight_max(as_usize(self.limits.max_in_flight)),
        );
        for chunk in wanted.chunks(batch_size(self.limits.batch_declarations)) {
            let texts: Vec<String> = chunk.iter().map(|one| one.text.clone()).collect();
            let embedded = model.models.embed_documents(texts, schedule).await?;
            let vectors = paired(chunk, embedded);
            self.vectors
                .store(&model.space.identity(), model.space.dimensions(), &vectors)
                .await
                .map_err(store_failed)?;
        }
        Ok(())
    }

    /// The vector ranking for `tree_revision`, or nothing when the tier cannot answer
    /// for that tree.
    ///
    /// The scan runs over the corpus the last pass published, and the query
    /// path reads no vector row of its own: doing that cost one `SELECT` over
    /// every stored vector and one decode of every blob it returned, per
    /// query. A held corpus with nothing in it ranks nothing, which is the
    /// answer this gave when the store held nothing.
    ///
    /// A corpus described for another tree ranks nothing either. Embedding runs after
    /// publication, so a workspace published moments ago is answered by the lexical tier
    /// alone until the pass for that tree lands, and never by the previous tree's vectors.
    ///
    /// One `tokio::task::spawn_blocking` call carries both the query's forward
    /// pass and the cosine scan over the held corpus. Neither may hold a
    /// runtime worker while another request waits, and running the two under
    /// one call schedules the work once rather than twice. The corpus travels
    /// into that call as the `Arc` this index holds, so the scan borrows the
    /// vectors rather than copying them.
    async fn vector_input(
        &self,
        query: &str,
        tree_revision: &str,
    ) -> Result<RankingInput, SearchError> {
        let unanswered = RankingInput::unanswered(RankingInputKind::Vector);
        let Some(model) = self.serving_model() else {
            return Ok(unanswered);
        };
        let Some(held) = self
            .held()
            .filter(|held| held.tree_revision == tree_revision)
        else {
            return Ok(unanswered);
        };
        if held.vectors.is_empty() {
            return Ok(unanswered);
        }
        let depth = self.limits.depth();
        let embedded = model.models.embed_query(query).await?;
        let scanned = Arc::clone(&held);
        let matched =
            tokio::task::spawn_blocking(move || nearest(&embedded, &scanned.vectors, depth))
                .await
                .map_err(task_failed)??;
        let placed = placed(&matched, &held.addresses);
        let spread = spread_per_file(&placed, as_usize(self.limits.per_file_max));
        let resolved = resolved(&spread, &held.addresses, as_usize(self.limits.candidates));
        Ok(RankingInput::new(
            RankingInputKind::Vector,
            resolved
                .into_iter()
                .map(|address| {
                    RankedIdentity::new(
                        address.identity,
                        FieldSet::of(SearchableField::DeclarationSource),
                    )
                })
                .collect(),
        ))
    }

    /// The model this index ranks through, when its readiness lets it answer.
    fn serving_model(&self) -> Option<Arc<LoadedModel>> {
        if !self.readiness().answers() {
            return None;
        }
        let held = self.model.lock().unwrap_or_else(PoisonError::into_inner);
        held.as_ref().map(Arc::clone)
    }

    /// What the last pass published, or nothing when no pass has published yet.
    fn held(&self) -> Option<Arc<HeldCorpus>> {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.as_ref().map(Arc::clone)
    }

    /// Publishes what this pass built, replacing the previous publication whole.
    ///
    /// One swap publishes the vectors, the map that places them, and the tree they were
    /// described for together, so no query can read one pass's vectors against another
    /// pass's map or against a tree neither answers for.
    fn publish(&self, documents: &[UnitDocument], vectors: Corpus, tree_revision: &str) {
        let addresses: Addresses = documents
            .iter()
            .map(|one| (one.digest.clone(), one.address.clone()))
            .collect();
        self.publish_held(Some(HeldCorpus {
            tree_revision: tree_revision.to_owned(),
            addresses,
            vectors,
        }));
    }

    /// Installs what the vector ranking scans from here on, or clears it.
    fn publish_held(&self, corpus: Option<HeldCorpus>) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        *held = corpus.map(Arc::new);
    }

    /// Records that a pass embedded nothing, without clearing a tier that is
    /// off or one that already failed for good.
    fn note_nothing_embedded(&self, total: usize) {
        if matches!(self.readiness(), VectorReadiness::Preparing { .. }) {
            self.set_readiness(VectorReadiness::Preparing {
                prepared: 0,
                total: as_count(total),
            });
        }
    }

    /// Records how far the vector ranking has got.
    fn set_readiness(&self, readiness: VectorReadiness) {
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
    let kept: Vec<&DescribedUnit<'_>> = described[..bound.min(described.len())]
        .iter()
        .filter(|one| UnitAddress::of(one.unit()).is_some())
        .collect();
    let texts: Vec<String> = kept
        .iter()
        .map(|one| document(one.declaration()).into_text())
        .collect();
    let keys = digests(&texts);
    kept.iter()
        .zip(texts)
        .zip(keys)
        .filter_map(|((one, text), key)| {
            Some(UnitDocument {
                address: UnitAddress::of(one.unit())?,
                digest: key.to_hex(),
                text,
            })
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

/// Each vector match under the file its declaration lives in.
///
/// The loop runs over what `nearest` returned, which is already the caller's
/// own depth bound.
fn placed(matched: &[VectorMatch], addresses: &Addresses) -> Vec<DeclarationMatch> {
    matched
        .iter()
        .filter_map(|one| {
            addresses
                .get(one.digest())
                .map(|address| DeclarationMatch::new(address.path.clone(), one.clone()))
        })
        .collect()
}

/// The units a spread vector ranking names, best first, keeping `keep_max`.
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

/// The readiness a pass that embedded `prepared` of `total` reaches.
const fn reached(prepared: u64, total: u64) -> VectorReadiness {
    if prepared == total {
        VectorReadiness::Ready
    } else {
        VectorReadiness::Preparing { prepared, total }
    }
}

/// The space one locally acquired model embeds into.
///
/// Public because the stored vectors are filed under this space's identity: a
/// caller inspecting the store, or a test writing rows into it, needs the same
/// value the index derives rather than a second spelling of it.
#[must_use]
///
/// A repository carries its revision: two revisions are two checkpoints, and
/// their vectors share no space. A directory carries the path it was read
/// from, which is the only revision a workspace-held model has.
pub fn local_embedding_space(source: &ModelSource, dimensions: usize) -> EmbeddingSpace {
    match source {
        ModelSource::Repository {
            repository,
            revision,
        } => EmbeddingSpace::local("hf", repository, revision, dimensions, false),
        ModelSource::Directory(directory) => EmbeddingSpace::local(
            "directory",
            directory.display().to_string(),
            directory.display().to_string(),
            dimensions,
            false,
        ),
    }
}

/// One store failure, with that store's own violation riding as the cause.
fn store_failed(source: LexicalIndexError) -> SearchError {
    let fault = SearchFault::new(SearchViolation::StoreFailed).carrying(source.fault());
    SearchError::new(fault.caused_by(source))
}

/// One blocking task that never returned what it was given to compute.
///
/// A join fails only when the task panicked or the runtime shut down under
/// it, so there is no encoder or store refusal to report and the join failure
/// itself is the evidence.
fn task_failed(source: tokio::task::JoinError) -> SearchError {
    let subject = source.to_string();
    let fault = SearchFault::new(SearchViolation::TaskFailed).about(subject);
    SearchError::new(fault.caused_by(source))
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
        Embedding, SearchIndexLimits, UnitAddress, UnitDocument, VectorReadiness, as_count,
        as_usize, batch_size, local_embedding_space, reached, selected,
    };
    use crate::acquisition::ModelSource;
    use rift_core::ProjectPath;
    use rift_index::LexicalIndexLimits;
    use rift_ranking::DocumentIdentity;
    use std::collections::BTreeSet;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn documented(digest: &str) -> Result<UnitDocument, Box<dyn std::error::Error>> {
        Ok(UnitDocument {
            address: UnitAddress {
                identity: DocumentIdentity::new(digest)?,
                path: ProjectPath::new("src/lib.rs".to_owned())?,
            },
            digest: digest.to_owned(),
            text: format!("fn {digest}"),
        })
    }

    #[test]
    fn test_a_pass_that_covered_every_declaration_is_ready() {
        assert_eq!(reached(0, 0), VectorReadiness::Ready);
        assert_eq!(reached(4, 4), VectorReadiness::Ready);
        assert_eq!(
            reached(2, 5),
            VectorReadiness::Preparing {
                prepared: 2,
                total: 5
            }
        );
    }

    #[test]
    fn test_readiness_answers_while_it_is_ready_or_preparing() {
        assert!(VectorReadiness::Ready.answers());
        assert!(
            VectorReadiness::Preparing {
                prepared: 1,
                total: 2
            }
            .answers()
        );
        assert!(!VectorReadiness::Disabled.answers());
        assert!(!VectorReadiness::Unavailable.answers());
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
    fn test_a_local_space_separates_revisions_widths_and_directories() -> TestResult {
        let repository =
            local_embedding_space(&ModelSource::repository("BAAI/bge-small-en-v1.5")?, 384);
        let pinned = local_embedding_space(
            &ModelSource::repository("BAAI/bge-small-en-v1.5@dd0a482")?,
            384,
        );
        assert_ne!(
            repository.identity(),
            pinned.identity(),
            "a pinned revision addresses its own space"
        );
        let widened =
            local_embedding_space(&ModelSource::repository("BAAI/bge-small-en-v1.5")?, 768);
        assert_ne!(
            repository.identity(),
            widened.identity(),
            "two widths are two spaces"
        );
        let directory = local_embedding_space(
            &ModelSource::Directory(std::path::PathBuf::from("models/bge")),
            384,
        );
        assert_ne!(repository.identity(), directory.identity());
        assert_eq!(directory.dimensions(), 384);
        Ok(())
    }

    #[test]
    fn test_the_builder_carries_every_bound_it_was_given() {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default())
            .candidates(9)
            .max_vectors(11)
            .batch_declarations(13)
            .max_tokens(64)
            .per_file_max(2)
            .disable_vector()
            .build();
        assert_eq!(limits.candidates(), 9);
        assert_eq!(limits.max_vectors(), 11);
        assert_eq!(limits.batch_declarations(), 13);
        assert_eq!(limits.max_tokens(), 64);
        assert_eq!(limits.per_file_max(), 2);
        assert!(limits.is_vector_disabled());
        assert_eq!(limits.lexical(), LexicalIndexLimits::default());
        assert_eq!(limits.initial_readiness(), VectorReadiness::Disabled);
        assert_eq!(limits.depth(), 18);
        assert_eq!(limits.encoder_limits().tokens_max(), 64);
        assert_eq!(limits.encoder_limits().batch_declarations(), 13);
        assert_eq!(limits.encoder_limits().texts_max(), 13);
    }

    #[test]
    fn test_the_shipped_bounds_start_preparing_with_nothing_embedded() {
        let limits = SearchIndexLimits::default();
        assert!(!limits.is_vector_disabled());
        assert_eq!(
            limits.initial_readiness(),
            VectorReadiness::Preparing {
                prepared: 0,
                total: 0
            }
        );
        assert_eq!(limits.depth(), 600);
    }
}

#[cfg(test)]
mod store_failure_tests {
    use super::store_failed;
    use rift_core::ProjectPath;
    use rift_index::{DatabasePool, LexicalIndexLimits, LexicalSearchIndex, WorkspaceDatabase};
    use rift_ranking::{
        DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument,
        SearchableField,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// The bound one document crosses, so the refusal carries limit evidence.
    fn one_unit_limits() -> LexicalIndexLimits {
        LexicalIndexLimits::new(1, 1 << 20, 32, 4, 1_000)
    }

    /// One symbol document, named and carrying its own declaration source.
    fn symbol(
        identity: &str,
        path: &str,
        name: &str,
        declaration_source: &str,
    ) -> Result<IndexDocument, Box<dyn std::error::Error>> {
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, name)
            .with(SearchableField::DeclarationSource, declaration_source);
        let digest = fields.digest();
        Ok(IndexDocument::new(
            DocumentIdentity::new(identity)?,
            DocumentLocation::Project(ProjectPath::new(path.to_owned())?),
            DocumentKind::Symbol,
            digest,
            fields,
        )?)
    }

    async fn refusal() -> Result<String, Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabasePool::new(4, 1_000))
                .await?;
        let index = LexicalSearchIndex::attached(database, one_unit_limits());
        let documents = [
            symbol(
                "rift://symbol/rust/a.rs/first",
                "a.rs",
                "first",
                "fn first() {}",
            )?,
            symbol(
                "rift://symbol/rust/a.rs/second",
                "a.rs",
                "second",
                "fn second() {}",
            )?,
        ];
        let refused = index
            .replace_all(&documents, "revision")
            .await
            .expect_err("two documents must cross a units_max of one");
        Ok(store_failed(refused).to_string())
    }

    #[tokio::test]
    async fn a_wrapped_store_refusal_states_its_action_once() -> TestResult {
        let message = refusal().await?;

        let action = "resize the request below the named limit";
        assert_eq!(
            message.matches(action).count(),
            1,
            "the action must appear once: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_wrapped_store_refusal_names_the_field_and_the_numbers() -> TestResult {
        let message = refusal().await?;

        assert!(message.contains("cause unit_limit"), "{message}");
        assert!(message.contains("field units_max"), "{message}");
        assert!(message.contains("observed 2"), "{message}");
        assert!(message.contains("maximum 1"), "{message}");
        Ok(())
    }

    #[tokio::test]
    async fn a_wrapped_store_refusal_repeats_no_explanation() -> TestResult {
        let message = refusal().await?;

        let explanation = "the request exceeded a declared resource limit";
        assert_eq!(
            message.matches(explanation).count(),
            1,
            "the explanation must appear once: {message}"
        );
        Ok(())
    }
}
