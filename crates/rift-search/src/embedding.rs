//! Model dispatch for the vector ranking.
//!
//! Rig owns the `EmbeddingModel` contract: one trait every model this crate can
//! run implements, so search, fusion, `SQLite`, and MCP call one method however
//! the vectors are produced. Adding a model family changes the loader and the
//! adapter tests alone.
//!
//! Rift keeps what Rig does not: acquiring the weights, running the local
//! encoder, scheduling batches under the operator's bounds, converting the
//! coordinates once at the boundary, and recording the space a corpus was
//! embedded into.
//!
//! The OpenAI-compatible arm builds on Rig's own client - its builder takes the
//! key, the base URL, and the workspace's timeout-configured HTTP client - but
//! decodes the response here. Rig 0.42.0 parses each response `index` and then
//! zips the response array with the input array positionally
//! (`src/providers/openai/embedding.rs`), so a service answering out of order
//! would attach every vector to the wrong declaration. Nothing in Rift's
//! contract makes array order meaningful: the response `index` decides, and a
//! duplicate, missing, or out-of-range index is refused.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use data_encoding::HEXLOWER;
use rig_core::embeddings::{Embedding, EmbeddingError, EmbeddingModel};
use rig_core::http_client::{Error as HttpClientError, HttpClientExt as _};
use rig_core::providers::openai;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::document::composition_revision;
use crate::encoder::Encoder;
use crate::error::{SearchError, SearchFault, SearchViolation};

/// Inputs one local embedding call may carry, at most.
///
/// This is the adapter's own ceiling, the widest batch the encoder accepts.
/// Runtime scheduling still applies the configured `batch_inputs`, which is
/// never larger.
pub const LOCAL_INPUTS_MAX: usize = 256;
/// Inputs one OpenAI-compatible embedding request may carry, at most.
pub const REMOTE_INPUTS_MAX: usize = 2_048;
/// The request path an OpenAI-compatible service answers embeddings at.
const EMBEDDINGS_PATH: &str = "/embeddings";
/// Characters one embedding-space identity renders as.
const SPACE_IDENTITY_CHARS: usize = 16;
/// The status a service answers when it timed the request out itself.
const REQUEST_TIMEOUT_STATUS: u16 = 408;
/// The status a service answers when the request arrived before it was ready.
const TOO_EARLY_STATUS: u16 = 425;
/// The status a service answers when the caller is over its rate limit.
const TOO_MANY_REQUESTS_STATUS: u16 = 429;
/// The first status a service answers its own failures with.
const SERVER_FAILURE_STATUS_MIN: u16 = 500;
/// The last status a service answers its own failures with.
const SERVER_FAILURE_STATUS_MAX: u16 = 599;

/// What a model does to a query before embedding it.
///
/// A symmetric model embeds a query the way it embeds a document. An
/// asymmetric one prefixes the query with the retrieval instruction its
/// checkpoint was trained on, so the two sides land in one space only when
/// the query carries that prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryTransformation {
    /// The query text reaches the model unchanged.
    Symmetric,
    /// The encoder prefixes the query with the retrieval instruction it
    /// carries.
    Instructed(&'static str),
}

impl QueryTransformation {
    /// The transformation as the space identity records it.
    ///
    /// An instructed model records the instruction itself, not the fact that
    /// there is one: a checkpoint whose prefix changes asks a different
    /// question of the same vectors, so it embeds into another space.
    fn as_str(self) -> String {
        match self {
            Self::Symmetric => "query".to_owned(),
            Self::Instructed(prefix) => format!("query-instruction:{prefix}"),
        }
    }
}

/// Where one set of vectors came from, and how.
///
/// Any change here invalidates the held corpus: two spaces are two coordinate
/// systems, and a query embedded in one ranks nothing in the other. The
/// revision is the resolved one - a repository's commit, a directory's file
/// digest - so a branch that moves under one name still mints another space,
/// and the document transformation is derived from what the builder composes
/// rather than declared, so changing the document changes the space.
/// Credentials never enter it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingSpace {
    kind: &'static str,
    origin: Option<String>,
    model: String,
    revision: String,
    dimensions: usize,
    document_transformation: String,
    query_transformation: String,
}

impl EmbeddingSpace {
    /// The space a locally run encoder embeds into.
    #[must_use]
    pub fn local(
        kind: &'static str,
        model: impl Into<String>,
        revision: impl Into<String>,
        dimensions: usize,
        query: QueryTransformation,
    ) -> Self {
        Self {
            kind,
            origin: None,
            model: model.into(),
            revision: revision.into(),
            dimensions,
            document_transformation: composition_revision(),
            query_transformation: query.as_str(),
        }
    }

    /// The space an OpenAI-compatible service embeds into.
    #[must_use]
    pub fn remote(
        origin: impl Into<String>,
        model: impl Into<String>,
        revision: impl Into<String>,
        dimensions: usize,
    ) -> Self {
        Self {
            kind: "openai_compatible",
            origin: Some(origin.into()),
            model: model.into(),
            revision: revision.into(),
            dimensions,
            document_transformation: composition_revision(),
            query_transformation: QueryTransformation::Symmetric.as_str(),
        }
    }

    /// The width every vector in this space carries.
    #[must_use]
    pub const fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// The stored identity the vector store files this space's vectors under.
    ///
    /// Every field the space is made of folds in, so a changed endpoint,
    /// model, revision, width, or transformation mints another identity and
    /// the previous vectors are dropped rather than scored against.
    #[must_use]
    pub fn identity(&self) -> String {
        let mut hasher = Sha256::new();
        for part in [
            self.kind,
            self.origin.as_deref().unwrap_or_default(),
            self.model.as_str(),
            self.revision.as_str(),
            self.document_transformation.as_str(),
            self.query_transformation.as_str(),
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hasher.update(self.dimensions.to_le_bytes());
        HEXLOWER.encode(&hasher.finalize())[..SPACE_IDENTITY_CHARS].to_owned()
    }
}

/// How one embedding pass is cut into requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchSchedule {
    batch_inputs: usize,
    max_in_flight: usize,
}

impl BatchSchedule {
    /// Names the inputs one request carries and the requests that may be open
    /// at once. Both are clamped to at least one, so a zero can never stall a
    /// pass.
    #[must_use]
    pub const fn new(batch_inputs: usize, max_in_flight: usize) -> Self {
        Self {
            batch_inputs: if batch_inputs == 0 { 1 } else { batch_inputs },
            max_in_flight: if max_in_flight == 0 { 1 } else { max_in_flight },
        }
    }

    /// One local pass: batches as configured, one request at a time, because
    /// the encoder holds a blocking thread while it runs.
    #[must_use]
    pub const fn local(batch_inputs: usize) -> Self {
        Self::new(batch_inputs, 1)
    }

    /// The inputs one request carries, further cut to what the model accepts.
    const fn batch_for(self, documents_max: usize) -> usize {
        if self.batch_inputs < documents_max {
            self.batch_inputs
        } else {
            documents_max
        }
    }
}

/// One model pair: the handle documents are embedded through and the handle
/// queries are embedded through.
///
/// The two are separate handles even when one loaded model serves both,
/// because a retrieval model may transform the two sides differently and a
/// later model must not be able to apply one transformation to both by
/// accident.
#[derive(Clone, Debug)]
pub struct RetrievalModels<DocumentModel, QueryModel> {
    documents: DocumentModel,
    query: QueryModel,
    space: EmbeddingSpace,
}

impl<DocumentModel, QueryModel> RetrievalModels<DocumentModel, QueryModel> {
    /// Pairs one document handle with one query handle in one space.
    #[must_use]
    pub const fn new(documents: DocumentModel, query: QueryModel, space: EmbeddingSpace) -> Self {
        Self {
            documents,
            query,
            space,
        }
    }

    /// The space this pair embeds into.
    #[must_use]
    pub const fn space(&self) -> &EmbeddingSpace {
        &self.space
    }
}

/// Every model family this crate can run.
///
/// One enum with concrete pairs rather than a trait object: Rig's
/// `EmbeddingModel` returns an opaque future and takes `impl IntoIterator`, so
/// it is not dyn-compatible. Each method matches once and hands the concrete
/// model to a generic helper, which is where the compiler picks the arm.
#[derive(Clone, Debug)]
pub enum EmbeddingModels {
    /// An encoder this process loaded and runs.
    Local(RetrievalModels<RiftLocalDocumentModel, RiftLocalQueryModel>),
    /// A service answering the `OpenAI` embedding shape.
    OpenAi(RetrievalModels<RiftOpenAiEmbeddingModel, RiftOpenAiEmbeddingModel>),
}

impl EmbeddingModels {
    /// The space this model embeds into.
    #[must_use]
    pub const fn space(&self) -> &EmbeddingSpace {
        match self {
            Self::Local(pair) => pair.space(),
            Self::OpenAi(pair) => pair.space(),
        }
    }

    /// Requests this model may have open at once.
    ///
    /// A locally run encoder holds a blocking thread for the length of a
    /// batch and candle's kernels already spread one pass across the thread
    /// pool, so running several buys nothing and costs the machine its
    /// memory. A remote service takes the operator's own bound.
    #[must_use]
    pub const fn requests_in_flight_max(&self, configured: usize) -> usize {
        match self {
            Self::Local(_) => 1,
            Self::OpenAi(_) => configured,
        }
    }

    /// Embeds every document, in the order it was handed over.
    ///
    /// # Errors
    ///
    /// Returns `encode_failed` when the model refuses, and
    /// `vector_width_mismatch` when a returned vector is not the space's own
    /// width or carries a value `f32` cannot hold.
    pub async fn embed_documents(
        &self,
        texts: Vec<String>,
        schedule: BatchSchedule,
    ) -> Result<Vec<Vec<f32>>, SearchError> {
        match self {
            Self::Local(pair) => embed_all(&pair.documents, texts, schedule, pair.space()).await,
            Self::OpenAi(pair) => embed_all(&pair.documents, texts, schedule, pair.space()).await,
        }
    }

    /// Embeds one query.
    ///
    /// # Errors
    ///
    /// Returns the same refusals [`Self::embed_documents`] does.
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, SearchError> {
        let schedule = BatchSchedule::new(1, 1);
        let embedded = match self {
            Self::Local(pair) => {
                embed_all(&pair.query, vec![text.to_owned()], schedule, pair.space()).await
            }
            Self::OpenAi(pair) => {
                embed_all(&pair.query, vec![text.to_owned()], schedule, pair.space()).await
            }
        }?;
        embedded
            .into_iter()
            .next()
            .ok_or_else(|| SearchError::new(SearchFault::new(SearchViolation::EncodeFailed)))
    }
}

/// Embeds `texts` through one concrete model, keeping every result in its own
/// input slot.
///
/// Batches are cut to the lower of the configured `batch_inputs` and the
/// model's own `MAX_DOCUMENTS`, and at most `max_in_flight` of them are open
/// at once. A completed batch returns to the slots its inputs came from, so
/// concurrency never reorders the answer.
///
/// The count is checked after the batches are joined as well as inside each
/// one: the remote arm refuses a wrong count per response, and this is where
/// a local model that answered a short batch is caught before a caller pairs
/// the vectors with its declarations by position.
async fn embed_all<Model>(
    model: &Model,
    texts: Vec<String>,
    schedule: BatchSchedule,
    space: &EmbeddingSpace,
) -> Result<Vec<Vec<f32>>, SearchError>
where
    Model: EmbeddingModel + Clone + Send + Sync + 'static,
{
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let batch = schedule.batch_for(Model::MAX_DOCUMENTS);
    let chunks: Vec<Vec<String>> = texts.chunks(batch).map(<[String]>::to_vec).collect();
    let mut placed: Vec<Option<Vec<Vec<f32>>>> = vec![None; chunks.len()];
    let mut running = tokio::task::JoinSet::new();
    for (position, chunk) in chunks.into_iter().enumerate() {
        if running.len() >= schedule.max_in_flight {
            collect_batch(&mut running, &mut placed).await?;
        }
        let model = model.clone();
        let width = space.dimensions();
        running.spawn(async move {
            let embedded = model.embed_texts(chunk).await;
            (position, embedded.map(|held| narrowed(held, width)))
        });
    }
    while !running.is_empty() {
        collect_batch(&mut running, &mut placed).await?;
    }
    let mut answered = Vec::with_capacity(texts.len());
    for held in placed {
        answered.extend(
            held.ok_or_else(|| SearchError::new(SearchFault::new(SearchViolation::EncodeFailed)))?,
        );
    }
    if answered.len() != texts.len() {
        return Err(SearchError::new(
            SearchFault::new(SearchViolation::EncodeFailed).about(format!(
                "{} vectors for {} inputs",
                answered.len(),
                texts.len()
            )),
        ));
    }
    Ok(answered)
}

/// The batch type one scheduled embedding task answers with.
type BatchAnswer = (
    usize,
    Result<Result<Vec<Vec<f32>>, SearchError>, EmbeddingError>,
);

/// Waits for one batch and files it under the position its inputs came from.
async fn collect_batch(
    running: &mut tokio::task::JoinSet<BatchAnswer>,
    placed: &mut [Option<Vec<Vec<f32>>>],
) -> Result<(), SearchError> {
    let Some(joined) = running.join_next().await else {
        return Ok(());
    };
    let (position, answered) = joined.map_err(task_failed)?;
    let embedded = answered.map_err(embedding_failed)??;
    placed[position] = Some(embedded);
    Ok(())
}

/// Narrows one batch's coordinates to the stored format, refusing a width the
/// space does not declare and a value `f32` cannot hold.
///
/// Rig returns `f64`. Rift stores finite `f32`, which is what the local index
/// already holds and what a later global store would carry, so the conversion
/// happens once here rather than at every read.
fn narrowed(embedded: Vec<Embedding>, width: usize) -> Result<Vec<Vec<f32>>, SearchError> {
    embedded
        .into_iter()
        .map(|one| {
            if one.vec.len() != width {
                return Err(SearchError::new(SearchFault::new(
                    SearchViolation::VectorWidthMismatch,
                )));
            }
            one.vec
                .into_iter()
                .map(|value| {
                    stored_coordinate(value).ok_or_else(|| {
                        SearchError::new(SearchFault::new(SearchViolation::VectorWidthMismatch))
                    })
                })
                .collect()
        })
        .collect()
}

/// One coordinate in the stored format, or `None` when the value cannot be
/// held there.
///
/// The stored format is `f32`: it is what the local index already holds and
/// what a later global store would carry, so a `f64` coordinate narrows once
/// here. A value past the narrower range becomes infinite and is refused
/// rather than stored as a coordinate no scan can use.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the stored format is f32; a value outside its range becomes infinite and is refused"
)]
fn stored_coordinate(value: f64) -> Option<f32> {
    let narrowed = value as f32;
    narrowed.is_finite().then_some(narrowed)
}

/// One blocking or scheduled task that did not return.
fn task_failed(source: tokio::task::JoinError) -> SearchError {
    SearchError::new(SearchFault::new(SearchViolation::TaskFailed).caused_by(source))
}

/// One model refusal, carried as this crate's own encode failure.
fn embedding_failed(source: EmbeddingError) -> SearchError {
    SearchError::new(SearchFault::new(SearchViolation::EncodeFailed).caused_by(source))
}

/// The already loaded encoder a local model handle is cloned from.
///
/// Acquiring the files, reading them, and detecting the family are fallible
/// and all finish before this exists. Rig's `make` is infallible, so it may
/// only clone a handle that is already ready.
#[derive(Clone, Debug)]
pub struct LocalEncoder {
    encoder: Arc<Encoder>,
}

impl LocalEncoder {
    /// Holds one loaded and validated encoder.
    #[must_use]
    pub const fn new(encoder: Arc<Encoder>) -> Self {
        Self { encoder }
    }

    /// The document handle for this encoder.
    #[must_use]
    pub fn documents(&self) -> RiftLocalDocumentModel {
        RiftLocalDocumentModel {
            encoder: Arc::clone(&self.encoder),
        }
    }

    /// The query handle for this encoder.
    #[must_use]
    pub fn query(&self) -> RiftLocalQueryModel {
        RiftLocalQueryModel {
            encoder: Arc::clone(&self.encoder),
        }
    }
}

/// The document side of a locally run encoder.
#[derive(Clone, Debug)]
pub struct RiftLocalDocumentModel {
    encoder: Arc<Encoder>,
}

/// The query side of a locally run encoder.
///
/// A BERT retrieval checkpoint prefixes the query with its own instruction and
/// leaves documents alone; a `Model2Vec` checkpoint treats the two alike. The
/// handle is separate either way, so the difference is the encoder's and never
/// the caller's to remember.
#[derive(Clone, Debug)]
pub struct RiftLocalQueryModel {
    encoder: Arc<Encoder>,
}

impl EmbeddingModel for RiftLocalDocumentModel {
    const MAX_DOCUMENTS: usize = LOCAL_INPUTS_MAX;

    type Client = LocalEncoder;

    fn make(client: &Self::Client, _model: impl Into<String>, _dims: Option<usize>) -> Self {
        client.documents()
    }

    fn ndims(&self) -> usize {
        self.encoder.dimension()
    }

    async fn embed_texts(
        &self,
        texts: impl IntoIterator<Item = String> + Send,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let texts: Vec<String> = texts.into_iter().collect();
        let encoder = Arc::clone(&self.encoder);
        let asked = texts.clone();
        let embedded = tokio::task::spawn_blocking(move || encoder.embed_documents(&asked))
            .await
            .map_err(joined)?
            .map_err(encoded)?;
        Ok(paired(texts, embedded))
    }
}

impl EmbeddingModel for RiftLocalQueryModel {
    const MAX_DOCUMENTS: usize = LOCAL_INPUTS_MAX;

    type Client = LocalEncoder;

    fn make(client: &Self::Client, _model: impl Into<String>, _dims: Option<usize>) -> Self {
        client.query()
    }

    fn ndims(&self) -> usize {
        self.encoder.dimension()
    }

    async fn embed_texts(
        &self,
        texts: impl IntoIterator<Item = String> + Send,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let texts: Vec<String> = texts.into_iter().collect();
        let encoder = Arc::clone(&self.encoder);
        let asked = texts.clone();
        let embedded = tokio::task::spawn_blocking(move || {
            asked
                .iter()
                .map(|text| encoder.embed_query(text))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(joined)?
        .map_err(encoded)?;
        Ok(paired(texts, embedded))
    }
}

/// Pairs each input with the vector the encoder produced for it, widening the
/// coordinates to the `f64` Rig's contract carries.
fn paired(texts: Vec<String>, embedded: Vec<Vec<f32>>) -> Vec<Embedding> {
    texts
        .into_iter()
        .zip(embedded)
        .map(|(document, values)| Embedding {
            document,
            vec: values.into_iter().map(f64::from).collect(),
        })
        .collect()
}

/// A blocking encoder task that did not return, as Rig's own failure.
fn joined(source: tokio::task::JoinError) -> EmbeddingError {
    EmbeddingError::DocumentError(Box::new(source))
}

/// One encoder refusal, as Rig's own failure.
fn encoded(source: SearchError) -> EmbeddingError {
    EmbeddingError::DocumentError(Box::new(source))
}

/// What one OpenAI-compatible endpoint needs before a model can be built.
///
/// The key is the credential itself, read from the environment by the layer
/// that accepted the configuration. `Debug` redacts it, so a trace of this
/// value can be recorded beside a failure.
#[derive(Clone)]
pub struct RemoteEmbeddingSettings {
    /// The base URL the request path is appended to.
    pub endpoint: String,
    /// The model the service exposes.
    pub model: String,
    /// The deployment identifier that participates in vector invalidation.
    pub revision: String,
    /// The width every returned vector must carry.
    pub dimensions: usize,
    /// The credential the request authenticates with.
    pub api_key: String,
    /// The wall-clock budget one request receives.
    pub request_timeout: Duration,
    /// Attempts one request makes before the pass gives up.
    pub attempts: u32,
}

impl std::fmt::Debug for RemoteEmbeddingSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteEmbeddingSettings")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("revision", &self.revision)
            .field("dimensions", &self.dimensions)
            .field("api_key", &"[redacted]")
            .field("request_timeout", &self.request_timeout)
            .field("attempts", &self.attempts)
            .finish()
    }
}

/// One model served by an endpoint speaking the `OpenAI` embedding shape.
///
/// Rig's client carries the credential, the base URL, and the workspace's
/// timeout-configured HTTP client. The request body and the response decode
/// are Rift's, because Rig 0.42.0 associates each returned vector with an
/// input by array position and ignores the `index` the service declared.
#[derive(Clone)]
pub struct RiftOpenAiEmbeddingModel {
    client: openai::Client<reqwest::Client>,
    model: String,
    dimensions: usize,
    attempts: u32,
}

impl std::fmt::Debug for RiftOpenAiEmbeddingModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RiftOpenAiEmbeddingModel")
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

impl RiftOpenAiEmbeddingModel {
    /// Builds one model over `settings`, through Rig's own client builder.
    ///
    /// The builder takes the credential, the configured base URL, and an HTTP
    /// client carrying the configured request timeout. Rig's
    /// `Client::from_env` is deliberately not used: it fixes the credential
    /// and endpoint variable names, which the `api_key_env` key exists to
    /// choose.
    ///
    /// # Errors
    ///
    /// Returns `model_source_invalid` when the HTTP client cannot be built or
    /// the endpoint cannot be used as a base URL. Neither arm is reachable from
    /// a test: `reqwest`'s builder fails only when its TLS backend cannot start,
    /// and Rig 0.42.0 never constructs `ClientBuilderError::InvalidProperty`, so
    /// its builder carries no validation of its own. Acceptance refuses a
    /// malformed endpoint before it reaches here.
    pub fn new(settings: &RemoteEmbeddingSettings) -> Result<Self, SearchError> {
        let http = reqwest::Client::builder()
            .timeout(settings.request_timeout)
            .build()
            .map_err(|source| {
                SearchError::new(
                    SearchFault::new(SearchViolation::ModelSourceInvalid).caused_by(source),
                )
            })?;
        let client = openai::Client::builder()
            .api_key(settings.api_key.clone())
            .base_url(settings.endpoint.clone())
            .http_client(http)
            .build()
            .map_err(|source| {
                SearchError::new(
                    SearchFault::new(SearchViolation::ModelSourceInvalid).caused_by(source),
                )
            })?;
        Ok(Self {
            client,
            model: settings.model.clone(),
            dimensions: settings.dimensions,
            attempts: settings.attempts,
        })
    }

    /// Issues one embedding request, trying again only where another attempt
    /// could answer differently.
    ///
    /// One attempt always runs. A refused attempt ends the request at once,
    /// because the service answers a malformed body, a rejected credential,
    /// and an unknown model the same way however often they are sent; a
    /// transient one is retried up to the configured attempt count.
    async fn request(&self, inputs: &[String]) -> Result<Vec<Vec<f64>>, EmbeddingError> {
        let mut attempted = self.attempt(inputs).await;
        let mut remaining = self.attempts.saturating_sub(1);
        while remaining > 0 && attempted.as_ref().is_err_and(AttemptFailure::is_transient) {
            attempted = self.attempt(inputs).await;
            remaining -= 1;
        }
        attempted.map_err(AttemptFailure::into_error)
    }

    /// One request and one decode.
    ///
    /// A non-success status never reaches a response here: the transport
    /// refuses it first (`rig_core::http_client`'s `into_lazy_response`), so
    /// the status the retry reads rides on the transport failure.
    async fn attempt(&self, inputs: &[String]) -> Result<Vec<Vec<f64>>, AttemptFailure> {
        let body = serde_json::to_vec(&EmbeddingRequestBody {
            model: &self.model,
            input: inputs,
            dimensions: self.dimensions,
        })
        .map_err(|error| AttemptFailure::refused(error.into()))?;
        let request = self
            .client
            .post(EMBEDDINGS_PATH)
            .map_err(|error| AttemptFailure::refused(error.into()))?
            .body(body)
            .map_err(|error| AttemptFailure::refused(EmbeddingError::HttpError(error.into())))?;
        let response = self
            .client
            .send(request)
            .await
            .map_err(|error| AttemptFailure::new(outcome_of(&error), error.into()))?;
        let received: Vec<u8> = response
            .into_body()
            .await
            .map_err(|error| AttemptFailure::transient(error.into()))?;
        let decoded: EmbeddingResponseBody = serde_json::from_slice(&received)
            .map_err(|error| AttemptFailure::refused(error.into()))?;
        placed_by_index(decoded.data, inputs.len()).map_err(AttemptFailure::refused)
    }
}

/// What one failed attempt says about trying again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    /// Another attempt could answer differently.
    Transient,
    /// The service refuses the request itself, and will answer every repeat
    /// of it the same way.
    Refused,
}

/// One failed attempt: what went wrong, and whether another is worth making.
#[derive(Debug)]
struct AttemptFailure {
    outcome: AttemptOutcome,
    error: EmbeddingError,
}

impl AttemptFailure {
    /// Names the outcome and the failure it carries.
    const fn new(outcome: AttemptOutcome, error: EmbeddingError) -> Self {
        Self { outcome, error }
    }

    /// A failure another attempt could answer differently.
    const fn transient(error: EmbeddingError) -> Self {
        Self::new(AttemptOutcome::Transient, error)
    }

    /// A failure every repeat of this request would meet again.
    const fn refused(error: EmbeddingError) -> Self {
        Self::new(AttemptOutcome::Refused, error)
    }

    /// Whether another attempt is worth making.
    const fn is_transient(&self) -> bool {
        matches!(self.outcome, AttemptOutcome::Transient)
    }

    /// The failure this attempt carries.
    fn into_error(self) -> EmbeddingError {
        self.error
    }
}

/// What one transport failure says about trying again.
///
/// A failure carrying no status is the request never reaching the service,
/// which another attempt can answer. A failure carrying one is classified by
/// [`outcome_of_status`].
fn outcome_of(error: &HttpClientError) -> AttemptOutcome {
    match status_of(error) {
        Some(status) => outcome_of_status(status),
        None => AttemptOutcome::Transient,
    }
}

/// The status one transport failure declares, absent when it carries none.
const fn status_of(error: &HttpClientError) -> Option<u16> {
    match error {
        HttpClientError::InvalidStatusCode(status)
        | HttpClientError::InvalidStatusCodeWithMessage(status, _)
        | HttpClientError::InvalidStatusCodeWithDetails { status, .. } => Some(status.as_u16()),
        _ => None,
    }
}

/// What one response status says about trying again.
///
/// The service declares a timeout, a rate limit, and its own failures as
/// statuses another attempt can answer. Every other status refuses the
/// request as sent: a malformed body, a rejected credential, a model the
/// service does not serve, and a width it does not accept all answer the
/// same way however often they are repeated.
const fn outcome_of_status(status: u16) -> AttemptOutcome {
    match status {
        REQUEST_TIMEOUT_STATUS | TOO_EARLY_STATUS | TOO_MANY_REQUESTS_STATUS => {
            AttemptOutcome::Transient
        }
        SERVER_FAILURE_STATUS_MIN..=SERVER_FAILURE_STATUS_MAX => AttemptOutcome::Transient,
        _ => AttemptOutcome::Refused,
    }
}

/// The request one OpenAI-compatible embedding call carries.
#[derive(serde::Serialize)]
struct EmbeddingRequestBody<'a> {
    model: &'a str,
    input: &'a [String],
    dimensions: usize,
}

/// The response one OpenAI-compatible embedding call answers with.
#[derive(Deserialize)]
struct EmbeddingResponseBody {
    data: Vec<EmbeddingDatum>,
}

/// One returned vector and the input position the service declared it for.
#[derive(Deserialize)]
struct EmbeddingDatum {
    index: usize,
    embedding: Vec<f64>,
}

/// Restores input order from the declared indexes.
///
/// The service may answer in any array order. Every input position is covered
/// exactly once or the response is refused: a duplicate index and one past
/// the request's own length both mean at least one declaration would take
/// another's vector, and a missing index is one of those two, because the
/// count is checked first.
fn placed_by_index(
    data: Vec<EmbeddingDatum>,
    inputs: usize,
) -> Result<Vec<Vec<f64>>, EmbeddingError> {
    if data.len() != inputs {
        return Err(EmbeddingError::ResponseError(format!(
            "the embedding response carries {} vectors for {inputs} inputs",
            data.len()
        )));
    }
    let mut placed: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    for datum in data {
        let index = datum.index;
        if index >= inputs || placed.insert(index, datum.embedding).is_some() {
            return Err(EmbeddingError::ResponseError(format!(
                "the embedding response declares index {index} for {inputs} inputs"
            )));
        }
    }
    // The response carries `inputs` vectors, every declared index is distinct
    // and below `inputs`, so the map holds one entry per input and its keys
    // ascend from zero: draining it in key order is the answer in input order.
    Ok(placed.into_values().collect())
}

impl EmbeddingModel for RiftOpenAiEmbeddingModel {
    const MAX_DOCUMENTS: usize = REMOTE_INPUTS_MAX;

    type Client = RemoteEmbeddingSettings;

    fn make(client: &Self::Client, model: impl Into<String>, dims: Option<usize>) -> Self {
        let mut settings = client.clone();
        settings.model = model.into();
        settings.dimensions = dims.unwrap_or(settings.dimensions);
        Self::new(&settings).unwrap_or_else(|error| {
            unreachable!("an accepted endpoint must build a client: error={error}")
        })
    }

    fn ndims(&self) -> usize {
        self.dimensions
    }

    async fn embed_texts(
        &self,
        texts: impl IntoIterator<Item = String> + Send,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let texts: Vec<String> = texts.into_iter().collect();
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let embedded = self.request(&texts).await?;
        Ok(texts
            .into_iter()
            .zip(embedded)
            .map(|(document, vec)| Embedding { document, vec })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AttemptOutcome, BatchSchedule, EmbeddingDatum, EmbeddingSpace, HttpClientError,
        LOCAL_INPUTS_MAX, QueryTransformation, RemoteEmbeddingSettings, RiftOpenAiEmbeddingModel,
        SearchViolation, embed_all, narrowed, outcome_of, outcome_of_status, placed_by_index,
        stored_coordinate,
    };
    use rig_core::embeddings::{Embedding, EmbeddingModel};
    use std::time::Duration;

    fn datum(index: usize, value: f64) -> EmbeddingDatum {
        EmbeddingDatum {
            index,
            embedding: vec![value],
        }
    }

    #[test]
    fn test_the_declared_indexes_restore_input_order() {
        let placed = placed_by_index(vec![datum(1, 2.0), datum(0, 1.0)], 2)
            .expect("a complete cover must be accepted");
        assert!((placed[0][0] - 1.0).abs() < 1e-12);
        assert!((placed[1][0] - 2.0).abs() < 1e-12);
    }

    #[test]
    fn test_a_repeated_index_is_refused() {
        assert!(placed_by_index(vec![datum(0, 1.0), datum(0, 2.0)], 2).is_err());
    }

    #[test]
    fn test_an_index_past_the_request_is_refused() {
        assert!(placed_by_index(vec![datum(0, 1.0), datum(2, 2.0)], 2).is_err());
    }

    #[test]
    fn test_a_count_the_request_did_not_ask_for_is_refused() {
        assert!(placed_by_index(vec![datum(0, 1.0)], 2).is_err());
        assert!(placed_by_index(vec![datum(0, 1.0), datum(1, 2.0)], 1).is_err());
    }

    #[test]
    fn test_an_empty_request_is_answered_by_an_empty_response() {
        assert!(
            placed_by_index(Vec::new(), 0)
                .expect("an empty cover must be accepted")
                .is_empty()
        );
    }

    #[test]
    fn test_a_coordinate_inside_the_stored_range_narrows() {
        assert!(stored_coordinate(1.5).is_some_and(|value| (value - 1.5).abs() < 1e-6));
        assert_eq!(stored_coordinate(f64::MAX), None);
        assert_eq!(stored_coordinate(f64::NAN), None);
    }

    #[test]
    fn test_a_width_the_space_does_not_declare_is_refused() {
        let embedded = vec![Embedding {
            document: "one".to_owned(),
            vec: vec![1.0, 2.0],
        }];
        assert!(narrowed(embedded.clone(), 2).is_ok());
        assert!(narrowed(embedded, 3).is_err());
    }

    #[test]
    fn test_a_schedule_never_stalls_on_a_zero() {
        let schedule = BatchSchedule::new(0, 0);
        assert_eq!(schedule.batch_for(LOCAL_INPUTS_MAX), 1);
        assert_eq!(BatchSchedule::local(32).max_in_flight, 1);
    }

    #[test]
    fn test_a_schedule_cuts_to_what_the_model_accepts() {
        assert_eq!(BatchSchedule::new(1_000, 1).batch_for(256), 256);
        assert_eq!(BatchSchedule::new(64, 1).batch_for(256), 64);
    }

    #[test]
    fn test_two_instructions_are_two_spaces() {
        // A checkpoint whose retrieval instruction changes asks a different
        // question of the same vectors, so the corpus embedded under the old
        // one must be dropped rather than scored against.
        let one = EmbeddingSpace::local(
            "hf",
            "model",
            "main",
            256,
            super::QueryTransformation::Instructed("first instruction: "),
        );
        let other = EmbeddingSpace::local(
            "hf",
            "model",
            "main",
            256,
            super::QueryTransformation::Instructed("second instruction: "),
        );
        assert_ne!(one.identity(), other.identity());
    }

    #[test]
    fn test_a_local_space_records_its_query_transformation() {
        let symmetric = EmbeddingSpace::local(
            "hf",
            "model",
            "main",
            256,
            super::QueryTransformation::Symmetric,
        );
        let instructed = EmbeddingSpace::local(
            "hf",
            "model",
            "main",
            256,
            super::QueryTransformation::Instructed("probe-instruction"),
        );
        assert_ne!(
            symmetric.identity(),
            instructed.identity(),
            "a query instruction is part of the space it embeds into"
        );
    }

    #[test]
    fn test_a_remote_space_records_its_origin() {
        let one = EmbeddingSpace::remote("https://one.example/v1", "model", "r", 8);
        let other = EmbeddingSpace::remote("https://two.example/v1", "model", "r", 8);
        assert_ne!(one.identity(), other.identity());
        assert_eq!(one.dimensions(), 8);
    }

    #[test]
    fn test_the_remote_settings_render_without_the_credential() {
        let settings = RemoteEmbeddingSettings {
            endpoint: "https://api.openai.com/v1".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            revision: "2024-01-25".to_owned(),
            dimensions: 1_536,
            api_key: "secret-value".to_owned(),
            request_timeout: Duration::from_secs(30),
            attempts: 3,
        };
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains("secret-value"));
        assert!(rendered.contains("[redacted]"));
    }

    fn remote_settings() -> RemoteEmbeddingSettings {
        RemoteEmbeddingSettings {
            endpoint: "https://api.openai.com/v1".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            revision: "2024-01-25".to_owned(),
            dimensions: 1_536,
            api_key: "secret-value".to_owned(),
            request_timeout: Duration::from_secs(30),
            attempts: 3,
        }
    }

    /// A model that answers one vector fewer than it was given, which is the
    /// only way a local handle can break the pairing a caller does by
    /// position.
    #[derive(Clone)]
    struct ShortModel;

    impl EmbeddingModel for ShortModel {
        const MAX_DOCUMENTS: usize = 8;

        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>, _dims: Option<usize>) -> Self {
            Self
        }

        fn ndims(&self) -> usize {
            1
        }

        fn embed_texts(
            &self,
            texts: impl IntoIterator<Item = String> + Send,
        ) -> impl Future<Output = Result<Vec<Embedding>, rig_core::embeddings::EmbeddingError>> + Send
        {
            let mut texts: Vec<String> = texts.into_iter().collect();
            texts.pop();
            std::future::ready(Ok(texts
                .into_iter()
                .map(|document| Embedding {
                    document,
                    vec: vec![1.0],
                })
                .collect()))
        }
    }

    #[tokio::test]
    async fn test_a_pass_that_answered_fewer_vectors_than_inputs_is_refused() {
        // The remote arm refuses a wrong count per response; this is the check
        // that catches a local handle doing the same, before a caller pairs
        // the vectors with its declarations by position.
        let space =
            EmbeddingSpace::local("directory", "short", "r", 1, QueryTransformation::Symmetric);
        let refused = embed_all(
            &ShortModel,
            vec!["one".to_owned(), "two".to_owned()],
            BatchSchedule::new(8, 1),
            &space,
        )
        .await
        .expect_err("a short answer must be refused");
        assert_eq!(refused.fault().violation(), SearchViolation::EncodeFailed);
        assert!(
            format!("{refused}").contains("1 vectors for 2 inputs"),
            "the refusal names both counts: {refused}"
        );
    }

    #[test]
    fn test_a_transport_failure_carrying_no_status_is_tried_again() {
        assert_eq!(
            outcome_of(&HttpClientError::NoHeaders),
            AttemptOutcome::Transient,
            "a request that never reached the service is worth another attempt"
        );
    }

    #[test]
    fn test_the_status_decides_whether_another_attempt_runs() {
        for status in [408, 425, 429, 500, 502, 503, 504, 599] {
            assert_eq!(
                outcome_of_status(status),
                AttemptOutcome::Transient,
                "status {status} is one another attempt can answer"
            );
        }
        for status in [400, 401, 403, 404, 409, 413, 422, 499] {
            assert_eq!(
                outcome_of_status(status),
                AttemptOutcome::Refused,
                "status {status} refuses the request as sent"
            );
        }
    }

    #[test]
    fn test_the_model_rig_builds_carries_the_name_and_width_it_was_given() {
        // Rift never calls `make`; the trait requires it, and an implementation
        // that dropped the arguments would be invisible until a Rig client of
        // this model's own type existed.
        let built = RiftOpenAiEmbeddingModel::make(&remote_settings(), "other-model", Some(256));
        let rendered = format!("{built:?}");
        assert!(rendered.contains("other-model"), "{rendered}");
        assert!(rendered.contains("256"), "{rendered}");
        let kept = RiftOpenAiEmbeddingModel::make(&remote_settings(), "other-model", None);
        assert!(format!("{kept:?}").contains("1536"));
    }

    #[test]
    fn test_the_remote_model_renders_without_the_credential() {
        let rendered = format!(
            "{:?}",
            RiftOpenAiEmbeddingModel::make(&remote_settings(), "text-embedding-3-small", None,)
        );
        assert!(!rendered.contains("secret-value"), "{rendered}");
    }
}
