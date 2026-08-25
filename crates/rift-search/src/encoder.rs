//! The encoder that turns declaration text into vectors.
//!
//! Everything here runs in-process: candle holds the weights, and one call
//! touches neither the network nor a subprocess. The weights are read through
//! `candle_core::safetensors::load` rather than memory-mapped, because mapping
//! them is an unsafe call and this workspace forbids authored `unsafe`; the
//! cost is that a load holds the file's bytes once while the tensors are built.
//!
//! A model's `config.json` names which of two paths it loads into. The BERT
//! path runs every layer of the model over one text's tokens and pools the CLS
//! vector. The static path has no transformer and no forward pass: it holds one
//! embedding row per token, gathers the rows a text's tokens address, and
//! averages them. Indexing a whole workspace on a laptop CPU costs minutes of
//! forward passes and seconds of row gathers, and that difference is what the
//! static path buys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use serde::Deserialize;
use tokenizers::models::ModelWrapper;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::error::{SearchError, SearchFault, SearchViolation};

/// The file a model's architecture and dimensions are read from.
pub(crate) const CONFIGURATION_FILE: &str = "config.json";
/// The file a model's tokenizer is read from.
pub(crate) const TOKENIZER_FILE: &str = "tokenizer.json";
/// The file a model's weights are read from.
pub(crate) const WEIGHTS_FILE: &str = "model.safetensors";

/// The prefix a retrieval query carries and a document does not.
///
/// The BERT checkpoint was trained with it on the query side alone. Omitting it
/// costs retrieval quality rather than failing, so it is applied where the query
/// enters instead of being left to each caller.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// The `model_type` a static embedding model names itself with.
const STATIC_MODEL_TYPE: &str = "model2vec";

/// The one tensor a static model ships: one embedding row per token.
const STATIC_WEIGHTS_TENSOR: &str = "embeddings";

/// The guard a normalization adds to the length it divides by.
///
/// A text the tokenizer empties has an all-zero vector whose length is zero, and
/// the guard keeps that division finite so the vector stays zero instead of
/// turning into NaN. The reference implementation divides by the same
/// `length + 1e-32`.
const NORMALIZATION_GUARD: f32 = 1e-32;

/// The three files an encoder loads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelFiles {
    configuration: PathBuf,
    tokenizer: PathBuf,
    weights: PathBuf,
}

impl ModelFiles {
    /// The three files below one model directory.
    ///
    /// # Errors
    ///
    /// Returns `model_file_missing` naming the first of the three that is
    /// absent, so an operator learns which file to supply rather than that the
    /// directory was rejected.
    pub fn in_directory(directory: &Path) -> Result<Self, SearchError> {
        let files = Self {
            configuration: directory.join(CONFIGURATION_FILE),
            tokenizer: directory.join(TOKENIZER_FILE),
            weights: directory.join(WEIGHTS_FILE),
        };
        let missing = [&files.configuration, &files.tokenizer, &files.weights]
            .into_iter()
            .find(|path| !path.is_file());
        match missing {
            Some(path) => Err(SearchError::new(
                SearchFault::new(SearchViolation::ModelFileMissing)
                    .about(path.display().to_string()),
            )),
            None => Ok(files),
        }
    }
}

/// What one encoder call may do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderLimits {
    batch_declarations: usize,
    tokens_max: usize,
    texts_max: usize,
}

impl EncoderLimits {
    /// Bounds one encoder: texts per forward pass, tokens per text, and texts
    /// per call.
    ///
    /// Attention memory grows with the square of `tokens_max`, so the two
    /// together bound what one pass holds; `texts_max` bounds the loop over
    /// passes so an unbounded caller cannot turn one call into a whole-tree
    /// build.
    #[must_use]
    pub const fn new(batch_declarations: usize, tokens_max: usize, texts_max: usize) -> Self {
        Self {
            batch_declarations,
            tokens_max,
            texts_max,
        }
    }

    /// Texts one forward pass takes.
    ///
    /// The static path runs no pass, so the same bound sets how many texts one
    /// tokenizer call takes there.
    #[must_use]
    pub const fn batch_declarations(self) -> usize {
        self.batch_declarations
    }

    /// Tokens the encoder reads from one text.
    #[must_use]
    pub const fn tokens_max(self) -> usize {
        self.tokens_max
    }

    /// Texts one call may hand the encoder.
    #[must_use]
    pub const fn texts_max(self) -> usize {
        self.texts_max
    }
}

/// The two paths one encoder loads into.
enum Model {
    /// The BERT path: every layer runs over the tokens, and the CLS vector of
    /// each sequence is the answer. The model is boxed because it is several
    /// times the width of the static path's own state, and one encoder holds
    /// one of the two.
    Bert(Box<BertModel>),
    /// The static path: one embedding row per token, gathered and averaged.
    Static(StaticWeights),
}

/// One loaded embedding model.
pub struct Encoder {
    model: Model,
    tokenizer: Tokenizer,
    device: Device,
    dimension: usize,
    limits: EncoderLimits,
}

impl std::fmt::Debug for Encoder {
    /// Names the model's shape. The loaded weights have no useful rendering and
    /// candle's own types do not implement `Debug`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Encoder")
            .field("dimension", &self.dimension)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Encoder {
    /// Loads one model from its three files, into the path its `config.json`
    /// names.
    ///
    /// A `config.json` whose `model_type` is `model2vec` loads into the static
    /// path, and every other model into the BERT path. That field is read
    /// before anything parses the rest of the file: a static model's
    /// `config.json` carries none of the fields candle's BERT configuration
    /// needs, so parsing it as one would refuse a model this encoder serves.
    ///
    /// # Errors
    ///
    /// Returns `model_configuration_invalid`, `tokenizer_unreadable`, or
    /// `weights_unreadable` naming the file that could not be read.
    pub fn load(files: &ModelFiles, limits: EncoderLimits) -> Result<Self, SearchError> {
        let text = std::fs::read_to_string(&files.configuration).map_err(|error| {
            failure(
                SearchViolation::ModelConfigurationInvalid,
                &files.configuration,
                error,
            )
        })?;
        if names_static_path(&text, &files.configuration)? {
            Self::load_static(files, &text, limits)
        } else {
            Self::load_bert(files, &text, limits)
        }
    }

    /// Loads the BERT path: the architecture from `config.json`, then every
    /// tensor that architecture names.
    fn load_bert(
        files: &ModelFiles,
        text: &str,
        limits: EncoderLimits,
    ) -> Result<Self, SearchError> {
        let configuration: Config = serde_json::from_str(text).map_err(|error| {
            failure(
                SearchViolation::ModelConfigurationInvalid,
                &files.configuration,
                error,
            )
        })?;
        let tokenizer = read_bert_tokenizer(&files.tokenizer, limits.tokens_max())?;
        let device = Device::Cpu;
        let weights = read_weights(&files.weights, &device)?;
        let variables = VarBuilder::from_tensors(weights, DType::F32, &device);
        let dimension = configuration.hidden_size;
        let model = BertModel::load(variables, &configuration)
            .map_err(|error| failure(SearchViolation::WeightsUnreadable, &files.weights, error))?;
        Ok(Self {
            model: Model::Bert(Box::new(model)),
            tokenizer,
            device,
            dimension,
            limits,
        })
    }

    /// Loads the static path: the embedding table, and the two flags the
    /// encoder reads from `config.json`.
    ///
    /// The width comes from the table's own second dimension. `hidden_dim`
    /// states that same number, so a `config.json` disagreeing with the weights
    /// it ships beside is refused instead of resolved in favor of either. A file
    /// that omits `hidden_dim` states nothing to disagree with.
    ///
    /// `apply_pca` and `apply_zipf` name transforms the training run already
    /// applied to these weights, so the static path applies nothing for them.
    fn load_static(
        files: &ModelFiles,
        text: &str,
        limits: EncoderLimits,
    ) -> Result<Self, SearchError> {
        let configuration: StaticConfiguration = serde_json::from_str(text).map_err(|error| {
            failure(
                SearchViolation::ModelConfigurationInvalid,
                &files.configuration,
                error,
            )
        })?;
        let tokenizer = read_static_tokenizer(&files.tokenizer)?;
        let weights = StaticWeights::read(
            &files.weights,
            configuration.normalize.unwrap_or(true),
            unknown_token(&tokenizer),
        )?;
        let declared = configuration.hidden_dim.unwrap_or(weights.width);
        if declared != weights.width {
            return Err(SearchError::new(
                SearchFault::new(SearchViolation::ModelConfigurationInvalid).about(format!(
                    "{}: hidden_dim {declared}, the weights hold {} columns",
                    files.configuration.display(),
                    weights.width
                )),
            ));
        }
        let dimension = weights.width;
        Ok(Self {
            model: Model::Static(weights),
            tokenizer,
            device: Device::Cpu,
            dimension,
            limits,
        })
    }

    /// The width of every vector this model produces.
    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }

    /// The bounds this encoder runs under.
    #[must_use]
    pub const fn limits(&self) -> EncoderLimits {
        self.limits
    }

    /// Embeds one retrieval query.
    ///
    /// The BERT path prefixes the query as its checkpoint was trained. The
    /// static path carries no prefix: it holds one row per token and has no
    /// query side of its own, so a query and a document run the same encoding.
    ///
    /// # Errors
    ///
    /// Returns `encode_failed` when tokenizing or the forward pass fails.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>, SearchError> {
        let text = match &self.model {
            Model::Bert(_) => format!("{QUERY_PREFIX}{query}"),
            Model::Static(_) => query.to_owned(),
        };
        let mut embedded = self.embed_documents(std::slice::from_ref(&text))?;
        match embedded.pop() {
            Some(vector) => Ok(vector),
            None => Err(SearchError::new(
                SearchFault::new(SearchViolation::EncodeFailed)
                    .about("one query produced no vector"),
            )),
        }
    }

    /// Embeds documents, unit length, in the order given.
    ///
    /// Passes run one at a time. Running several through the rayon pool put a
    /// whole machine into memory pressure during the spike, because each pass
    /// holds its own activations; candle's own kernels already spread one pass
    /// across the pool.
    ///
    /// The loop is bounded by the check above it: at most
    /// [`EncoderLimits::texts_max`] texts arrive, so at most
    /// `texts_max / batch_declarations` passes run.
    ///
    /// # Errors
    ///
    /// Returns `text_limit` when the call carries more texts than
    /// [`EncoderLimits::texts_max`], and `encode_failed` when a pass fails.
    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SearchError> {
        if texts.len() > self.limits.texts_max() {
            return Err(SearchError::new(
                SearchFault::new(SearchViolation::TextLimit).about(format!(
                    "{} texts, {} allowed",
                    texts.len(),
                    self.limits.texts_max()
                )),
            ));
        }
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.limits.batch_declarations()) {
            vectors.extend(self.embed_batch(batch)?);
        }
        Ok(vectors)
    }

    /// One batch through the path this encoder loaded.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SearchError> {
        match &self.model {
            Model::Bert(model) => self.embed_batch_bert(model, texts),
            Model::Static(weights) => self.embed_batch_static(weights, texts),
        }
    }

    /// One forward pass over one batch, pooled and normalized.
    fn embed_batch_bert(
        &self,
        model: &BertModel,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, SearchError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|error| boxed_encode_failure("tokenizing a batch", error.as_ref()))?;
        let width = encodings
            .first()
            .map_or(0, |encoding| encoding.get_ids().len());
        if width == 0 {
            return Err(SearchError::new(
                SearchFault::new(SearchViolation::EncodeFailed)
                    .about("the tokenizer produced no tokens"),
            ));
        }
        let shape = (texts.len(), width);
        let ids: Vec<u32> = encodings
            .iter()
            .flat_map(|one| one.get_ids().to_vec())
            .collect();
        let mask: Vec<u32> = encodings
            .iter()
            .flat_map(|one| one.get_attention_mask().to_vec())
            .collect();
        self.pool(model, &ids, &mask, shape)
    }

    /// One batch through the static path: gather, average, normalize.
    ///
    /// The tokenizer runs with special tokens disabled, as the reference
    /// implementation does, so no `[CLS]` or `[SEP]` row enters a mean the
    /// training never put one in. Each text is gathered on its own, so no text
    /// in the batch is padded to the length of another.
    fn embed_batch_static(
        &self,
        weights: &StaticWeights,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, SearchError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), false)
            .map_err(|error| boxed_encode_failure("tokenizing a batch", error.as_ref()))?;
        let mut vectors = Vec::with_capacity(encodings.len());
        for encoding in &encodings {
            vectors.push(weights.embed(encoding.get_ids(), self.limits.tokens_max())?);
        }
        Ok(vectors)
    }

    /// The CLS vector of each sequence, unit length.
    ///
    /// BGE checkpoints pool by the CLS token rather than by mean, and the CLS
    /// vector is the first position of every sequence.
    fn pool(
        &self,
        model: &BertModel,
        ids: &[u32],
        mask: &[u32],
        shape: (usize, usize),
    ) -> Result<Vec<Vec<f32>>, SearchError> {
        let build = |values: &[u32]| Tensor::from_slice(values, shape, &self.device);
        let ids = build(ids).map_err(|error| encode_failure("building the token tensor", error))?;
        let mask =
            build(mask).map_err(|error| encode_failure("building the mask tensor", error))?;
        let types = ids
            .zeros_like()
            .map_err(|error| encode_failure("building the token-type tensor", error))?;
        let hidden = model
            .forward(&ids, &types, Some(&mask))
            .map_err(|error| encode_failure("the forward pass", error))?;
        let pooled = hidden
            .i((.., 0))
            .and_then(|cls| cls.broadcast_div(&cls.sqr()?.sum_keepdim(1)?.sqrt()?))
            .map_err(|error| encode_failure("pooling the CLS vector", error))?;
        pooled
            .to_dtype(DType::F32)
            .and_then(|pooled| pooled.to_vec2::<f32>())
            .map_err(|error| encode_failure("reading the pooled vectors", error))
    }
}

/// The fields the static path reads from a model2vec `config.json`.
#[derive(Deserialize)]
struct StaticConfiguration {
    /// The width the model states. The weights state it too, and the encoder
    /// refuses a disagreement.
    hidden_dim: Option<usize>,
    /// Whether the model asks for unit-length vectors. The reference
    /// implementation reads it as true where the file omits it.
    normalize: Option<bool>,
}

/// The one field of a `config.json` that selects the path.
#[derive(Deserialize)]
struct ModelType {
    model_type: Option<String>,
}

/// The embedding table the static path gathers from.
struct StaticWeights {
    /// Every row laid end to end: row `id` starts at `id * width`. A flat
    /// buffer keeps one gather a slice lookup, so an id past the last row is a
    /// `None` this module turns into a refusal rather than a panic.
    rows: Vec<f32>,
    /// The rows the table holds, named in the refusal an out-of-range id gets.
    count: usize,
    /// The width of one row, and of every vector this path produces.
    width: usize,
    /// The id the tokenizer answers unknown text with, dropped before the mean.
    unknown_token: Option<u32>,
    /// Whether `config.json` asks for unit-length vectors.
    normalize: bool,
}

impl StaticWeights {
    /// Reads the one tensor a static model ships, row by row.
    ///
    /// candle builds the tensor and this reads it straight into a `Vec<f32>`:
    /// the static path addresses rows by index and runs no kernel over them, so
    /// holding the table as a tensor would cost a candle call per token and buy
    /// nothing.
    fn read(path: &Path, normalize: bool, unknown_token: Option<u32>) -> Result<Self, SearchError> {
        let device = Device::Cpu;
        let mut tensors = read_weights(path, &device)?;
        let table = tensors.remove(STATIC_WEIGHTS_TENSOR).ok_or_else(|| {
            SearchError::new(
                SearchFault::new(SearchViolation::WeightsUnreadable).about(format!(
                    "{}: the weights hold no `{STATIC_WEIGHTS_TENSOR}` tensor",
                    path.display()
                )),
            )
        })?;
        let (count, width) = table
            .dims2()
            .map_err(|error| failure(SearchViolation::WeightsUnreadable, path, error))?;
        if count == 0 || width == 0 {
            return Err(SearchError::new(
                SearchFault::new(SearchViolation::WeightsUnreadable).about(format!(
                    "{}: the `{STATIC_WEIGHTS_TENSOR}` tensor is {count} by {width}",
                    path.display()
                )),
            ));
        }
        let rows = table
            .to_dtype(DType::F32)
            .and_then(|table| table.flatten_all())
            .and_then(|table| table.to_vec1::<f32>())
            .map_err(|error| failure(SearchViolation::WeightsUnreadable, path, error))?;
        Ok(Self {
            rows,
            count,
            width,
            unknown_token,
            normalize,
        })
    }

    /// The mean of the rows one text's tokens address, unit length where the
    /// model asks for it.
    ///
    /// The steps run in the reference implementation's order: the unknown token
    /// goes first, and what survives is cut at `tokens_max`. Cutting first would
    /// drop tokens the reference keeps, because the ids it removes still hold
    /// places in the sequence.
    ///
    /// A text that keeps no token embeds as zeros of the model's width.
    ///
    /// The loop is bounded by that cut: it runs `tokens_max` times at most,
    /// whatever the tokenizer returned.
    fn embed(&self, ids: &[u32], tokens_max: usize) -> Result<Vec<f32>, SearchError> {
        let mut total = vec![0.0_f32; self.width];
        let mut kept = 0.0_f32;
        let carried = ids
            .iter()
            .copied()
            .filter(|id| Some(*id) != self.unknown_token)
            .take(tokens_max);
        for id in carried {
            for (sum, value) in total.iter_mut().zip(self.row(id)?) {
                *sum += *value;
            }
            kept += 1.0;
        }
        if kept > 0.0 {
            for value in &mut total {
                *value /= kept;
            }
        }
        if self.normalize {
            to_unit_length(&mut total);
        }
        Ok(total)
    }

    /// The row one token id addresses.
    ///
    /// A tokenizer whose vocabulary outruns the table it was shipped with
    /// addresses a row that is not there, and the encoder reports that rather
    /// than skipping the token or indexing past the buffer.
    fn row(&self, id: u32) -> Result<&[f32], SearchError> {
        let start = usize::try_from(id)
            .ok()
            .and_then(|index| index.checked_mul(self.width));
        let row = start
            .and_then(|start| Some(start..start.checked_add(self.width)?))
            .and_then(|range| self.rows.get(range));
        row.ok_or_else(|| {
            SearchError::new(
                SearchFault::new(SearchViolation::EncodeFailed).about(format!(
                    "token {id} is past the {} rows the weights hold",
                    self.count
                )),
            )
        })
    }
}

/// Whether a `config.json` names a static embedding model.
fn names_static_path(text: &str, path: &Path) -> Result<bool, SearchError> {
    let named: ModelType = serde_json::from_str(text)
        .map_err(|error| failure(SearchViolation::ModelConfigurationInvalid, path, error))?;
    Ok(named.model_type.as_deref() == Some(STATIC_MODEL_TYPE))
}

/// Divides a vector by its own length.
fn to_unit_length(vector: &mut [f32]) {
    let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    let divisor = length + NORMALIZATION_GUARD;
    for value in &mut *vector {
        *value /= divisor;
    }
}

/// The id the tokenizer answers unknown text with, where its model names one.
///
/// The static path drops that id: the row behind it says nothing about the text
/// that produced it, and averaging it in pulls every unfamiliar text toward one
/// point. `tokenizers` keeps a Unigram model's `unk_id` private, so a Unigram
/// tokenizer reports none here and every id it produces reaches the mean.
fn unknown_token(tokenizer: &Tokenizer) -> Option<u32> {
    let token = match tokenizer.get_model() {
        ModelWrapper::BPE(model) => model.get_unk_token().clone(),
        ModelWrapper::WordPiece(model) => Some(model.unk_token.clone()),
        ModelWrapper::WordLevel(model) => Some(model.unk_token.clone()),
        ModelWrapper::Unigram(_) => None,
    };
    token.and_then(|token| tokenizer.token_to_id(&token))
}

/// The tensors of one model, read once into memory.
fn read_weights(path: &Path, device: &Device) -> Result<HashMap<String, Tensor>, SearchError> {
    candle_core::safetensors::load(path, device)
        .map_err(|error| failure(SearchViolation::WeightsUnreadable, path, error))
}

/// The model's tokenizer, padding to the batch's longest and truncating at
/// `tokens_max`.
///
/// The BERT path runs one rectangle of tokens through the model, so every
/// sequence in a pass is padded to one width and the mask tells the model which
/// positions the padding filled.
fn read_bert_tokenizer(path: &Path, tokens_max: usize) -> Result<Tokenizer, SearchError> {
    let mut tokenizer = open_tokenizer(path)?;
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        ..PaddingParams::default()
    }));
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: tokens_max,
            ..TruncationParams::default()
        }))
        .map_err(|error| tokenizer_failure(path, error.as_ref()))?;
    Ok(tokenizer)
}

/// The model's tokenizer, neither padding nor truncating.
///
/// The static path reads one text's ids at a time, so a padding row would enter
/// a mean it does not belong in. Truncating here would cut real tokens before
/// the unknown ids are dropped, and the encoder cuts after that drop instead.
/// What the tokenizer returns is bounded by the text the caller handed over.
fn read_static_tokenizer(path: &Path) -> Result<Tokenizer, SearchError> {
    let mut tokenizer = open_tokenizer(path)?;
    tokenizer.with_padding(None);
    tokenizer
        .with_truncation(None)
        .map_err(|error| tokenizer_failure(path, error.as_ref()))?;
    Ok(tokenizer)
}

/// The model's tokenizer, as the file states it.
fn open_tokenizer(path: &Path) -> Result<Tokenizer, SearchError> {
    Tokenizer::from_file(path).map_err(|error| tokenizer_failure(path, error.as_ref()))
}

/// One file-reading failure, naming the file and its cause.
fn failure(
    violation: SearchViolation,
    path: &Path,
    source: impl std::error::Error + Send + Sync + 'static,
) -> SearchError {
    SearchError::new(
        SearchFault::new(violation)
            .about(path.display().to_string())
            .caused_by(source),
    )
}

/// One tokenizer failure, whose cause is boxed rather than typed upstream.
fn tokenizer_failure(path: &Path, source: &(dyn std::error::Error + Send + Sync)) -> SearchError {
    SearchError::new(
        SearchFault::new(SearchViolation::TokenizerUnreadable)
            .about(format!("{}: {source}", path.display())),
    )
}

/// One encoding failure whose cause is boxed rather than typed upstream.
fn boxed_encode_failure(
    stage: &str,
    source: &(dyn std::error::Error + Send + Sync),
) -> SearchError {
    SearchError::new(
        SearchFault::new(SearchViolation::EncodeFailed).about(format!("{stage}: {source}")),
    )
}

/// One encoding failure, naming the stage that failed.
fn encode_failure(stage: &str, source: candle_core::Error) -> SearchError {
    SearchError::new(
        SearchFault::new(SearchViolation::EncodeFailed)
            .about(stage.to_owned())
            .caused_by(source),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use candle_core::{DType, Device, Tensor};
    use tokenizers::models::wordpiece::WordPiece;
    use tokenizers::processors::bert::BertProcessing;
    use tokenizers::{Tokenizer, normalizers, pre_tokenizers};

    use super::{Encoder, EncoderLimits, ModelFiles, boxed_encode_failure, encode_failure};
    use crate::error::SearchViolation;

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
    type Loaded = Result<Encoder, Box<dyn std::error::Error + Send + Sync>>;

    /// The fixture vocabulary: the three tokens a BERT tokenizer needs, the two
    /// words the tests embed, then every word of the query prefix. The prefix
    /// words are in it so a prefix that reaches the tokenizer moves the answer
    /// instead of resolving to the unknown token.
    const WORDS: [&str; 12] = [
        "[UNK]",
        "[CLS]",
        "[SEP]",
        "alpha",
        "beta",
        "this",
        "represent",
        "sentence",
        "for",
        "searching",
        "relevant",
        "passages",
    ];
    /// The fixture's width, shared by both paths.
    const WIDTH: usize = 4;
    /// One static row per token of [`WORDS`]. The special rows and the prefix
    /// rows are far from the two word rows, so a mean that took one in would
    /// miss every expectation below by a wide margin.
    const ROWS: [[f32; WIDTH]; 12] = [
        [9.0, 9.0, 9.0, 9.0],
        [7.0, 7.0, 7.0, 7.0],
        [5.0, 5.0, 5.0, 5.0],
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 2.0, 0.0, 0.0],
        [0.0, 0.0, 3.0, 0.0],
        [8.0, 0.0, 0.0, 0.0],
        [0.0, 8.0, 0.0, 0.0],
        [0.0, 0.0, 8.0, 0.0],
        [0.0, 0.0, 0.0, 8.0],
        [6.0, 6.0, 0.0, 0.0],
        [0.0, 6.0, 6.0, 0.0],
    ];
    /// The fixture BERT model's layers, heads, and inner width.
    const LAYERS: usize = 1;
    const HEADS: usize = 2;
    const INTERMEDIATE: usize = 8;
    const POSITIONS: usize = 32;
    const TYPES: usize = 2;

    fn limits() -> EncoderLimits {
        EncoderLimits::new(2, 16, 8)
    }

    /// Fails unless two vectors agree within what an f32 mean and a
    /// normalization leave.
    fn assert_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(
            actual.len(),
            expected.len(),
            "widths differ: {actual:?} against {expected:?}"
        );
        for (one, other) in actual.iter().zip(expected) {
            assert!(
                (one - other).abs() < 1e-6,
                "{actual:?} against {expected:?}"
            );
        }
    }

    /// Writes the tokenizer both paths load: `WordPiece` over [`WORDS`], with the
    /// processor that adds `[CLS]` and `[SEP]` where special tokens are asked
    /// for.
    fn write_tokenizer(directory: &Path) -> TestResult {
        let vocabulary = directory.join("vocab.txt");
        std::fs::write(&vocabulary, format!("{}\n", WORDS.join("\n")))?;
        let model = WordPiece::from_file(vocabulary.to_str().unwrap_or_default())
            .unk_token("[UNK]".to_owned())
            .build()?;
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_normalizer(Some(normalizers::BertNormalizer::default()));
        tokenizer.with_pre_tokenizer(Some(pre_tokenizers::bert::BertPreTokenizer));
        tokenizer.with_post_processor(Some(BertProcessing::new(
            ("[SEP]".to_owned(), 2),
            ("[CLS]".to_owned(), 1),
        )));
        tokenizer.save(directory.join("tokenizer.json"), false)?;
        Ok(())
    }

    fn write_static_configuration(
        directory: &Path,
        hidden_dim: Option<usize>,
        normalize: bool,
    ) -> TestResult {
        let mut configuration = serde_json::json!({
            "model_type": "model2vec",
            "architectures": ["StaticModel"],
            "tokenizer_name": "baai/bge-base-en-v1.5",
            "apply_pca": 512,
            "apply_zipf": true,
            "seq_length": 1_000_000,
            "normalize": normalize,
        });
        if let Some(width) = hidden_dim {
            configuration["hidden_dim"] = serde_json::json!(width);
        }
        std::fs::write(
            directory.join("config.json"),
            serde_json::to_vec_pretty(&configuration)?,
        )?;
        Ok(())
    }

    fn write_static_weights(directory: &Path, rows: &[[f32; WIDTH]]) -> TestResult {
        let values: Vec<f32> = rows.iter().flatten().copied().collect();
        let table = Tensor::from_vec(values, (rows.len(), WIDTH), &Device::Cpu)?;
        let mut tensors: HashMap<String, Tensor> = HashMap::new();
        tensors.insert("embeddings".to_owned(), table);
        candle_core::safetensors::save(&tensors, directory.join("model.safetensors"))?;
        Ok(())
    }

    fn static_model(directory: &Path, hidden_dim: Option<usize>, normalize: bool) -> TestResult {
        write_static_configuration(directory, hidden_dim, normalize)?;
        write_tokenizer(directory)?;
        write_static_weights(directory, &ROWS)
    }

    fn static_encoder(directory: &Path, limits: EncoderLimits) -> Loaded {
        static_model(directory, Some(WIDTH), true)?;
        let files = ModelFiles::in_directory(directory)?;
        Ok(Encoder::load(&files, limits)?)
    }

    fn write_bert_configuration(directory: &Path) -> TestResult {
        let configuration = serde_json::json!({
            "vocab_size": WORDS.len(),
            "hidden_size": WIDTH,
            "num_hidden_layers": LAYERS,
            "num_attention_heads": HEADS,
            "intermediate_size": INTERMEDIATE,
            "hidden_act": "gelu",
            "hidden_dropout_prob": 0.0,
            "max_position_embeddings": POSITIONS,
            "type_vocab_size": TYPES,
            "initializer_range": 0.02,
            "layer_norm_eps": 1e-12,
            "pad_token_id": 0,
            "model_type": "bert",
        });
        std::fs::write(
            directory.join("config.json"),
            serde_json::to_vec_pretty(&configuration)?,
        )?;
        Ok(())
    }

    /// Deterministic weights: identity layer norms so a pooled vector is finite,
    /// and small varying values everywhere else so two texts differ.
    fn write_bert_weights(directory: &Path) -> TestResult {
        let device = Device::Cpu;
        let mut tensors: HashMap<String, Tensor> = HashMap::new();
        let varying = |rows: usize, columns: usize| -> Result<Tensor, candle_core::Error> {
            let values: Vec<f32> = (0..rows * columns)
                .map(|index| f32::from(u8::try_from(index % 17).unwrap_or_default()))
                .map(|step| step.mul_add(0.01, -0.08))
                .collect();
            Tensor::from_vec(values, (rows, columns), &device)
        };
        tensors.insert(
            "embeddings.word_embeddings.weight".to_owned(),
            varying(WORDS.len(), WIDTH)?,
        );
        tensors.insert(
            "embeddings.position_embeddings.weight".to_owned(),
            varying(POSITIONS, WIDTH)?,
        );
        tensors.insert(
            "embeddings.token_type_embeddings.weight".to_owned(),
            varying(TYPES, WIDTH)?,
        );
        insert_layer_norm(&mut tensors, "embeddings.LayerNorm", &device)?;
        let base = "encoder.layer.0";
        for projection in ["query", "key", "value"] {
            let prefix = format!("{base}.attention.self.{projection}");
            tensors.insert(format!("{prefix}.weight"), varying(WIDTH, WIDTH)?);
            tensors.insert(
                format!("{prefix}.bias"),
                Tensor::zeros(WIDTH, DType::F32, &device)?,
            );
        }
        let attention_output = format!("{base}.attention.output");
        tensors.insert(
            format!("{attention_output}.dense.weight"),
            varying(WIDTH, WIDTH)?,
        );
        tensors.insert(
            format!("{attention_output}.dense.bias"),
            Tensor::zeros(WIDTH, DType::F32, &device)?,
        );
        insert_layer_norm(
            &mut tensors,
            &format!("{attention_output}.LayerNorm"),
            &device,
        )?;
        tensors.insert(
            format!("{base}.intermediate.dense.weight"),
            varying(INTERMEDIATE, WIDTH)?,
        );
        tensors.insert(
            format!("{base}.intermediate.dense.bias"),
            Tensor::zeros(INTERMEDIATE, DType::F32, &device)?,
        );
        tensors.insert(
            format!("{base}.output.dense.weight"),
            varying(WIDTH, INTERMEDIATE)?,
        );
        tensors.insert(
            format!("{base}.output.dense.bias"),
            Tensor::zeros(WIDTH, DType::F32, &device)?,
        );
        insert_layer_norm(&mut tensors, &format!("{base}.output.LayerNorm"), &device)?;
        candle_core::safetensors::save(&tensors, directory.join("model.safetensors"))?;
        Ok(())
    }

    fn insert_layer_norm(
        tensors: &mut HashMap<String, Tensor>,
        prefix: &str,
        device: &Device,
    ) -> TestResult {
        tensors.insert(
            format!("{prefix}.weight"),
            Tensor::ones(WIDTH, DType::F32, device)?,
        );
        tensors.insert(
            format!("{prefix}.bias"),
            Tensor::zeros(WIDTH, DType::F32, device)?,
        );
        Ok(())
    }

    fn bert_encoder(directory: &Path) -> Loaded {
        write_bert_configuration(directory)?;
        write_tokenizer(directory)?;
        write_bert_weights(directory)?;
        let files = ModelFiles::in_directory(directory)?;
        Ok(Encoder::load(&files, limits())?)
    }

    #[test]
    fn test_limits_report_what_they_were_built_with() {
        let limits = EncoderLimits::new(32, 256, 4096);
        assert_eq!(limits.batch_declarations(), 32);
        assert_eq!(limits.tokens_max(), 256);
        assert_eq!(limits.texts_max(), 4096);
    }

    #[test]
    fn test_encode_failure_names_its_stage_and_keeps_the_candle_cause() {
        let cause = candle_core::Error::Msg("shape mismatch".to_owned());
        let error = encode_failure("the forward pass", cause);
        assert_eq!(error.fault().violation(), SearchViolation::EncodeFailed);
        let rendered = error.to_string();
        assert!(rendered.contains("encode_failed"), "{rendered}");
        assert!(rendered.contains("the forward pass"), "{rendered}");
        assert!(
            std::error::Error::source(&error).is_some(),
            "the candle failure rides as the source"
        );
    }

    #[test]
    fn test_boxed_encode_failure_folds_its_cause_into_the_subject() {
        let cause = std::io::Error::other("vocabulary missing");
        let error = boxed_encode_failure("tokenizing a batch", &cause);
        assert_eq!(error.fault().violation(), SearchViolation::EncodeFailed);
        let rendered = error.to_string();
        assert!(rendered.contains("tokenizing a batch"), "{rendered}");
        assert!(
            rendered.contains("vocabulary missing"),
            "a boxed cause has no typed source, so it is folded into the subject: {rendered}"
        );
    }

    #[test]
    fn test_the_static_path_averages_the_rows_its_tokens_address() -> TestResult {
        let directory = tempfile::tempdir()?;
        let encoder = static_encoder(directory.path(), limits())?;
        assert_eq!(encoder.dimension(), WIDTH);
        let vectors = encoder.embed_documents(&["alpha beta".to_owned()])?;
        assert_eq!(vectors.len(), 1);
        assert_close(&vectors[0], &[0.447_213_6, 0.894_427_2, 0.0, 0.0]);
        Ok(())
    }

    #[test]
    fn test_the_static_path_drops_the_unknown_token_before_it_averages() -> TestResult {
        let directory = tempfile::tempdir()?;
        let encoder = static_encoder(directory.path(), limits())?;
        let vectors = encoder.embed_documents(&["alpha gamma".to_owned(), "alpha".to_owned()])?;
        assert_close(&vectors[0], &[1.0, 0.0, 0.0, 0.0]);
        assert_close(&vectors[0], &vectors[1]);
        let length: f32 = vectors[0].iter().map(|value| value * value).sum();
        assert!(
            (length - 1.0).abs() < 1e-6,
            "a normalized vector is unit length, got {length}"
        );
        Ok(())
    }

    #[test]
    fn test_a_text_that_keeps_no_token_embeds_as_zeros_rather_than_nan() -> TestResult {
        let directory = tempfile::tempdir()?;
        let encoder = static_encoder(directory.path(), limits())?;
        let vectors = encoder.embed_documents(&[String::new(), "gamma".to_owned()])?;
        for vector in &vectors {
            assert!(
                vector.iter().all(|value| value.is_finite()),
                "the guard keeps the division finite: {vector:?}"
            );
            assert_close(vector, &[0.0; WIDTH]);
        }
        Ok(())
    }

    #[test]
    fn test_a_model_that_asks_for_no_normalization_keeps_the_raw_mean() -> TestResult {
        let directory = tempfile::tempdir()?;
        static_model(directory.path(), Some(WIDTH), false)?;
        let files = ModelFiles::in_directory(directory.path())?;
        let encoder = Encoder::load(&files, limits())?;
        let vectors = encoder.embed_documents(&["alpha beta".to_owned()])?;
        assert_close(&vectors[0], &[0.5, 1.0, 0.0, 0.0]);
        Ok(())
    }

    #[test]
    fn test_tokens_past_the_bound_do_not_reach_the_mean() -> TestResult {
        let directory = tempfile::tempdir()?;
        let encoder = static_encoder(directory.path(), EncoderLimits::new(2, 2, 8))?;
        let vectors =
            encoder.embed_documents(&["alpha beta alpha".to_owned(), "alpha beta".to_owned()])?;
        assert_close(&vectors[0], &vectors[1]);
        assert_close(&vectors[0], &[0.447_213_6, 0.894_427_2, 0.0, 0.0]);
        Ok(())
    }

    #[test]
    fn test_a_query_on_the_static_path_carries_no_prefix() -> TestResult {
        let directory = tempfile::tempdir()?;
        let encoder = static_encoder(directory.path(), limits())?;
        let query = encoder.embed_query("alpha")?;
        let document = encoder.embed_documents(&["alpha".to_owned()])?;
        assert_close(&query, &document[0]);
        assert_close(&query, &[1.0, 0.0, 0.0, 0.0]);
        Ok(())
    }

    #[test]
    fn test_a_token_past_the_weights_rows_is_refused() -> TestResult {
        let directory = tempfile::tempdir()?;
        write_static_configuration(directory.path(), Some(WIDTH), true)?;
        write_tokenizer(directory.path())?;
        write_static_weights(directory.path(), &ROWS[..4])?;
        let files = ModelFiles::in_directory(directory.path())?;
        let encoder = Encoder::load(&files, limits())?;
        let error = encoder
            .embed_documents(&["beta".to_owned()])
            .expect_err("the beta row is not in the table");
        assert_eq!(error.fault().violation(), SearchViolation::EncodeFailed);
        let rendered = error.to_string();
        assert!(rendered.contains("token 4"), "{rendered}");
        assert!(rendered.contains("4 rows"), "{rendered}");
        Ok(())
    }

    #[test]
    fn test_a_declared_width_that_disagrees_with_the_weights_is_refused() -> TestResult {
        let directory = tempfile::tempdir()?;
        static_model(directory.path(), Some(WIDTH * 2), true)?;
        let files = ModelFiles::in_directory(directory.path())?;
        let error = Encoder::load(&files, limits())
            .expect_err("the configuration and the weights state two widths");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::ModelConfigurationInvalid
        );
        let rendered = error.to_string();
        assert!(rendered.contains("hidden_dim 8"), "{rendered}");
        assert!(rendered.contains("4 columns"), "{rendered}");
        Ok(())
    }

    #[test]
    fn test_a_static_model_without_its_tensor_is_refused() -> TestResult {
        let directory = tempfile::tempdir()?;
        static_model(directory.path(), None, true)?;
        let mut tensors: HashMap<String, Tensor> = HashMap::new();
        tensors.insert(
            "vectors".to_owned(),
            Tensor::zeros((2, WIDTH), DType::F32, &Device::Cpu)?,
        );
        candle_core::safetensors::save(&tensors, directory.path().join("model.safetensors"))?;
        let files = ModelFiles::in_directory(directory.path())?;
        let error = Encoder::load(&files, limits()).expect_err("the table is under another name");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::WeightsUnreadable
        );
        assert!(error.to_string().contains("`embeddings`"), "{error}");
        Ok(())
    }

    #[test]
    fn test_the_model_type_selects_the_path_the_weights_load_into() -> TestResult {
        let directory = tempfile::tempdir()?;
        static_encoder(directory.path(), limits())?;
        write_bert_configuration(directory.path())?;
        let files = ModelFiles::in_directory(directory.path())?;
        let error = Encoder::load(&files, limits())
            .expect_err("the BERT path needs every tensor its architecture names");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::WeightsUnreadable
        );
        Ok(())
    }

    #[test]
    fn test_a_model_that_names_another_type_still_loads_and_pools_as_bert() -> TestResult {
        let directory = tempfile::tempdir()?;
        let encoder = bert_encoder(directory.path())?;
        assert_eq!(encoder.dimension(), WIDTH);
        let documents =
            encoder.embed_documents(&["alpha beta".to_owned(), "beta alpha".to_owned()])?;
        let query = encoder.embed_query("alpha beta")?;
        for vector in documents.iter().chain(std::iter::once(&query)) {
            assert_eq!(vector.len(), WIDTH);
            let length: f32 = vector.iter().map(|value| value * value).sum();
            assert!(
                (length - 1.0).abs() < 1e-4,
                "a pooled vector is unit length, got {length}"
            );
        }
        Ok(())
    }
}
