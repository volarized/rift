//! The BERT encoder that turns declaration text into vectors.
//!
//! Everything here runs in-process: candle holds the weights, and one call
//! touches neither the network nor a subprocess. The weights are read through
//! `candle_core::safetensors::load` rather than memory-mapped, because mapping
//! them is an unsafe call and this workspace forbids authored `unsafe`; the
//! cost is that a load holds the file's bytes once while the tensors are built.

use std::path::{Path, PathBuf};

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::error::{SearchError, SearchFault, SearchViolation};

/// The file a model's architecture and dimensions are read from.
const CONFIGURATION_FILE: &str = "config.json";
/// The file a model's tokenizer is read from.
const TOKENIZER_FILE: &str = "tokenizer.json";
/// The file a model's weights are read from.
const WEIGHTS_FILE: &str = "model.safetensors";

/// The prefix a retrieval query carries and a document does not.
///
/// The model was trained with it on the query side alone. Omitting it costs
/// retrieval quality rather than failing, so it is applied where the query
/// enters instead of being left to each caller.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

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

/// One loaded embedding model.
pub struct Encoder {
    model: BertModel,
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
    /// Loads one model from its three files.
    ///
    /// # Errors
    ///
    /// Returns `model_configuration_invalid`, `tokenizer_unreadable`, or
    /// `weights_unreadable` naming the file that could not be read.
    pub fn load(files: &ModelFiles, limits: EncoderLimits) -> Result<Self, SearchError> {
        let configuration = read_configuration(&files.configuration)?;
        let tokenizer = read_tokenizer(&files.tokenizer, limits.tokens_max())?;
        let device = Device::Cpu;
        let weights = candle_core::safetensors::load(&files.weights, &device)
            .map_err(|error| failure(SearchViolation::WeightsUnreadable, &files.weights, error))?;
        let variables = VarBuilder::from_tensors(weights, DType::F32, &device);
        let dimension = configuration.hidden_size;
        let model = BertModel::load(variables, &configuration)
            .map_err(|error| failure(SearchViolation::WeightsUnreadable, &files.weights, error))?;
        Ok(Self {
            model,
            tokenizer,
            device,
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

    /// Embeds one retrieval query, prefixed as the model was trained.
    ///
    /// # Errors
    ///
    /// Returns `encode_failed` when tokenizing or the forward pass fails.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>, SearchError> {
        let text = format!("{QUERY_PREFIX}{query}");
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

    /// One forward pass over one batch, pooled and normalized.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SearchError> {
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
        self.pool(&ids, &mask, shape)
    }

    /// The CLS vector of each sequence, unit length.
    ///
    /// BGE checkpoints pool by the CLS token rather than by mean, and the CLS
    /// vector is the first position of every sequence.
    fn pool(
        &self,
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
        let hidden = self
            .model
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

/// The model's architecture and dimensions, from its `config.json`.
fn read_configuration(path: &Path) -> Result<Config, SearchError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| failure(SearchViolation::ModelConfigurationInvalid, path, error))?;
    serde_json::from_str(&text)
        .map_err(|error| failure(SearchViolation::ModelConfigurationInvalid, path, error))
}

/// The model's tokenizer, padding to the batch's longest and truncating at
/// `tokens_max`.
fn read_tokenizer(path: &Path, tokens_max: usize) -> Result<Tokenizer, SearchError> {
    let mut tokenizer =
        Tokenizer::from_file(path).map_err(|error| tokenizer_failure(path, error.as_ref()))?;
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
    use super::{EncoderLimits, boxed_encode_failure, encode_failure};
    use crate::error::SearchViolation;

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
}
