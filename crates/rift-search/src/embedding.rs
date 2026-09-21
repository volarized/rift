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

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use data_encoding::HEXLOWER;
use rig_core::embeddings::{Embedding, EmbeddingError, EmbeddingModel};
use rig_core::http_client::HttpClientExt as _;
use rig_core::providers::openai;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

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
/// The document transformation the local and remote arms alike apply: the
/// document text reaches the model unchanged.
const DOCUMENT_TRANSFORMATION: &str = "document";
/// The query transformation a symmetric model applies: the query text reaches
/// the model unchanged.
const SYMMETRIC_QUERY_TRANSFORMATION: &str = "query";
/// The query transformation an asymmetric local model applies: the encoder
/// prefixes the query with its retrieval instruction.
const INSTRUCTED_QUERY_TRANSFORMATION: &str = "query-instruction";
/// Characters one embedding-space identity renders as.
const SPACE_IDENTITY_CHARS: usize = 16;

/// Where one set of vectors came from, and how.
///
/// Any change here invalidates the held corpus: two spaces are two coordinate
/// systems, and a query embedded in one ranks nothing in the other.
/// Credentials never enter it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingSpace {
    kind: &'static str,
    origin: Option<String>,
    model: String,
    revision: String,
    dimensions: usize,
    document_transformation: &'static str,
    query_transformation: &'static str,
}

impl EmbeddingSpace {
    /// The space a locally run encoder embeds into.
    #[must_use]
    pub fn local(
        kind: &'static str,
        model: impl Into<String>,
        revision: impl Into<String>,
        dimensions: usize,
        instructed_query: bool,
    ) -> Self {
        Self {
            kind,
            origin: None,
            model: model.into(),
            revision: revision.into(),
            dimensions,
            document_transformation: DOCUMENT_TRANSFORMATION,
            query_transformation: if instructed_query {
                INSTRUCTED_QUERY_TRANSFORMATION
            } else {
                SYMMETRIC_QUERY_TRANSFORMATION
            },
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
            document_transformation: DOCUMENT_TRANSFORMATION,
            query_transformation: SYMMETRIC_QUERY_TRANSFORMATION,
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
            self.document_transformation,
            self.query_transformation,
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
    /// the endpoint cannot be used as a base URL.
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
            attempts: settings.attempts.max(1),
        })
    }

    /// Issues one embedding request, retrying a refusal up to the configured
    /// attempt count.
    async fn request(&self, inputs: &[String]) -> Result<Vec<Vec<f64>>, EmbeddingError> {
        let mut refusal = None;
        for _attempt in 0..self.attempts {
            match self.attempt(inputs).await {
                Ok(embedded) => return Ok(embedded),
                Err(error) => refusal = Some(error),
            }
        }
        Err(refusal.unwrap_or_else(|| {
            EmbeddingError::ResponseError("no embedding attempt ran".to_owned())
        }))
    }

    /// One request and one decode.
    async fn attempt(&self, inputs: &[String]) -> Result<Vec<Vec<f64>>, EmbeddingError> {
        let body = serde_json::to_vec(&EmbeddingRequestBody {
            model: &self.model,
            input: inputs,
            dimensions: self.dimensions,
        })?;
        let request = self
            .client
            .post(EMBEDDINGS_PATH)?
            .body(body)
            .map_err(|error| EmbeddingError::HttpError(error.into()))?;
        let response = self.client.send(request).await?;
        let status = response.status();
        let received: Vec<u8> = response.into_body().await?;
        if !status.is_success() {
            return Err(EmbeddingError::from_http_response(
                status,
                String::from_utf8_lossy(&received).into_owned(),
            ));
        }
        let decoded: EmbeddingResponseBody = serde_json::from_slice(&received)?;
        placed_by_index(decoded.data, inputs.len())
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
/// exactly once or the response is refused: a duplicate index, a missing one,
/// and one past the request's own length all mean at least one declaration
/// would take another's vector.
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
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut placed: Vec<Option<Vec<f64>>> = vec![None; inputs];
    for datum in data {
        if datum.index >= inputs || !seen.insert(datum.index) {
            return Err(EmbeddingError::ResponseError(format!(
                "the embedding response declares index {} for {inputs} inputs",
                datum.index
            )));
        }
        placed[datum.index] = Some(datum.embedding);
    }
    placed
        .into_iter()
        .enumerate()
        .map(|(position, held)| {
            held.ok_or_else(|| {
                EmbeddingError::ResponseError(format!(
                    "the embedding response declares no vector for input {position}"
                ))
            })
        })
        .collect()
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
        BatchSchedule, EmbeddingDatum, EmbeddingSpace, LOCAL_INPUTS_MAX, RemoteEmbeddingSettings,
        narrowed, placed_by_index, stored_coordinate,
    };
    use rig_core::embeddings::Embedding;
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
    fn test_a_local_space_records_its_query_transformation() {
        let symmetric = EmbeddingSpace::local("hf", "model", "main", 256, false);
        let instructed = EmbeddingSpace::local("hf", "model", "main", 256, true);
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
}
