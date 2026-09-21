//! The encoder against a model built in the test, so no suite touches the network.

use std::collections::HashMap;
use std::path::Path;

use candle_core::{DType, Device, Tensor};
use rift_search::{Declaration, Encoder, EncoderLimits, ModelFiles, SearchViolation, document};
use tokenizers::models::wordpiece::WordPiece;
use tokenizers::processors::bert::BertProcessing;
use tokenizers::{Tokenizer, normalizers, pre_tokenizers};

/// The fixture model's width, layers, and attention heads. Small enough that a
/// forward pass costs nothing and large enough to exercise every tensor the
/// architecture loads.
const HIDDEN: usize = 8;
const LAYERS: usize = 2;
const HEADS: usize = 2;
const INTERMEDIATE: usize = 16;
const POSITIONS: usize = 32;
const TYPES: usize = 2;

/// The fixture vocabulary: the two special tokens the processor needs, then
/// words the tests embed.
const WORDS: [&str; 8] = [
    "[UNK]", "[CLS]", "[SEP]", "load", "config", "read", "search", "index",
];

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Writes one loadable model into `directory`.
fn write_model(directory: &Path) -> TestResult {
    write_configuration(directory)?;
    write_tokenizer(directory)?;
    write_weights(directory)
}

fn write_configuration(directory: &Path) -> TestResult {
    let configuration = serde_json::json!({
        "vocab_size": WORDS.len(),
        "hidden_size": HIDDEN,
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

fn write_tokenizer(directory: &Path) -> TestResult {
    write_tokenizer_with(directory, true)
}

/// Writes the tokenizer, optionally without the post-processor that adds
/// `[CLS]` and `[SEP]`. Without it an empty string encodes to no tokens at all,
/// which is the only way to reach the encoder's empty-batch refusal.
fn write_tokenizer_with(directory: &Path, post_process: bool) -> TestResult {
    let vocabulary = directory.join("vocab.txt");
    std::fs::write(&vocabulary, format!("{}\n", WORDS.join("\n")))?;
    let model = WordPiece::from_file(vocabulary.to_str().unwrap_or_default())
        .unk_token("[UNK]".to_owned())
        .build()?;
    let mut tokenizer = Tokenizer::new(model);
    tokenizer.with_normalizer(Some(normalizers::BertNormalizer::default()));
    tokenizer.with_pre_tokenizer(Some(pre_tokenizers::bert::BertPreTokenizer));
    if post_process {
        tokenizer.with_post_processor(Some(BertProcessing::new(
            ("[SEP]".to_owned(), 2),
            ("[CLS]".to_owned(), 1),
        )));
    }
    tokenizer.save(directory.join("tokenizer.json"), false)?;
    Ok(())
}

/// Deterministic weights: identity-ish layer norms so a pooled vector is finite,
/// and small varying values everywhere else so two texts differ.
fn write_weights(directory: &Path) -> TestResult {
    let device = Device::Cpu;
    let mut tensors: HashMap<String, Tensor> = HashMap::new();
    let mut varying = |rows: usize, columns: usize| -> Result<Tensor, candle_core::Error> {
        let values: Vec<f32> = (0..rows * columns)
            .map(|index| f32::from(u8::try_from(index % 17).unwrap_or_default()))
            .map(|step| step.mul_add(0.01, -0.08))
            .collect();
        Tensor::from_vec(values, (rows, columns), &device)
    };
    tensors.insert(
        "embeddings.word_embeddings.weight".to_owned(),
        varying(WORDS.len(), HIDDEN)?,
    );
    tensors.insert(
        "embeddings.position_embeddings.weight".to_owned(),
        varying(POSITIONS, HIDDEN)?,
    );
    tensors.insert(
        "embeddings.token_type_embeddings.weight".to_owned(),
        varying(TYPES, HIDDEN)?,
    );
    insert_layer_norm(&mut tensors, "embeddings.LayerNorm", HIDDEN, &device)?;
    for layer in 0..LAYERS {
        insert_layer(&mut tensors, layer, &mut varying, &device)?;
    }
    candle_core::safetensors::save(&tensors, directory.join("model.safetensors"))?;
    Ok(())
}

fn insert_layer(
    tensors: &mut HashMap<String, Tensor>,
    layer: usize,
    varying: &mut impl FnMut(usize, usize) -> Result<Tensor, candle_core::Error>,
    device: &Device,
) -> TestResult {
    let base = format!("encoder.layer.{layer}");
    for projection in ["query", "key", "value"] {
        let prefix = format!("{base}.attention.self.{projection}");
        tensors.insert(format!("{prefix}.weight"), varying(HIDDEN, HIDDEN)?);
        tensors.insert(
            format!("{prefix}.bias"),
            Tensor::zeros(HIDDEN, DType::F32, device)?,
        );
    }
    let attention_output = format!("{base}.attention.output");
    tensors.insert(
        format!("{attention_output}.dense.weight"),
        varying(HIDDEN, HIDDEN)?,
    );
    tensors.insert(
        format!("{attention_output}.dense.bias"),
        Tensor::zeros(HIDDEN, DType::F32, device)?,
    );
    insert_layer_norm(
        tensors,
        &format!("{attention_output}.LayerNorm"),
        HIDDEN,
        device,
    )?;
    tensors.insert(
        format!("{base}.intermediate.dense.weight"),
        varying(INTERMEDIATE, HIDDEN)?,
    );
    tensors.insert(
        format!("{base}.intermediate.dense.bias"),
        Tensor::zeros(INTERMEDIATE, DType::F32, device)?,
    );
    tensors.insert(
        format!("{base}.output.dense.weight"),
        varying(HIDDEN, INTERMEDIATE)?,
    );
    tensors.insert(
        format!("{base}.output.dense.bias"),
        Tensor::zeros(HIDDEN, DType::F32, device)?,
    );
    insert_layer_norm(tensors, &format!("{base}.output.LayerNorm"), HIDDEN, device)?;
    Ok(())
}

fn insert_layer_norm(
    tensors: &mut HashMap<String, Tensor>,
    prefix: &str,
    width: usize,
    device: &Device,
) -> TestResult {
    tensors.insert(
        format!("{prefix}.weight"),
        Tensor::ones(width, DType::F32, device)?,
    );
    tensors.insert(
        format!("{prefix}.bias"),
        Tensor::zeros(width, DType::F32, device)?,
    );
    Ok(())
}

fn limits() -> EncoderLimits {
    EncoderLimits::new(2, 16, 8)
}

fn loaded(directory: &Path) -> Result<Encoder, Box<dyn std::error::Error + Send + Sync>> {
    write_model(directory)?;
    let files = ModelFiles::in_directory(directory)?;
    Ok(Encoder::load(&files, limits())?)
}

#[test]
fn a_model_directory_missing_a_file_names_the_file() -> TestResult {
    let directory = tempfile::tempdir()?;
    write_model(directory.path())?;
    std::fs::remove_file(directory.path().join("tokenizer.json"))?;
    let error = ModelFiles::in_directory(directory.path()).expect_err("the file is gone");
    assert!(
        error.to_string().contains("tokenizer.json"),
        "the refusal names the missing file: {error}"
    );
    Ok(())
}

#[test]
fn an_empty_directory_is_refused_as_a_missing_model_file() -> TestResult {
    let directory = tempfile::tempdir()?;
    let error = ModelFiles::in_directory(directory.path()).expect_err("nothing is there");
    assert!(error.to_string().contains("model_file_missing"), "{error}");
    Ok(())
}

#[test]
fn unreadable_weights_are_refused_without_a_panic() -> TestResult {
    let directory = tempfile::tempdir()?;
    write_model(directory.path())?;
    std::fs::write(
        directory.path().join("model.safetensors"),
        b"not safetensors",
    )?;
    let files = ModelFiles::in_directory(directory.path())?;
    let error = Encoder::load(&files, limits()).expect_err("the weights are not weights");
    assert!(error.to_string().contains("weights_unreadable"), "{error}");
    Ok(())
}

#[test]
fn an_invalid_configuration_is_refused_without_a_panic() -> TestResult {
    let directory = tempfile::tempdir()?;
    write_model(directory.path())?;
    std::fs::write(directory.path().join("config.json"), b"{")?;
    let files = ModelFiles::in_directory(directory.path())?;
    let error = Encoder::load(&files, limits()).expect_err("the configuration is truncated");
    assert!(
        error.to_string().contains("model_configuration_invalid"),
        "{error}"
    );
    Ok(())
}

#[test]
fn an_unreadable_tokenizer_is_refused_without_a_panic() -> TestResult {
    let directory = tempfile::tempdir()?;
    write_model(directory.path())?;
    std::fs::write(directory.path().join("tokenizer.json"), b"{")?;
    let files = ModelFiles::in_directory(directory.path())?;
    let error = Encoder::load(&files, limits()).expect_err("the tokenizer is truncated");
    assert!(
        error.to_string().contains("tokenizer_unreadable"),
        "{error}"
    );
    Ok(())
}

#[test]
fn documents_embed_to_unit_vectors_of_the_models_width() -> TestResult {
    let directory = tempfile::tempdir()?;
    let encoder = loaded(directory.path())?;
    assert_eq!(encoder.dimension(), HIDDEN);
    let texts = vec![
        document(&Declaration::new("fn", "load_config").source("fn load config")).into_text(),
        document(&Declaration::new("fn", "read_index").source("fn read index")).into_text(),
    ];
    let vectors = encoder.embed_documents(&texts)?;
    assert_eq!(vectors.len(), 2);
    for vector in &vectors {
        assert_eq!(vector.len(), HIDDEN);
        let length: f32 = vector.iter().map(|value| value * value).sum();
        assert!(
            (length - 1.0).abs() < 1e-4,
            "a pooled vector is unit length, got {length}"
        );
    }
    Ok(())
}

#[test]
fn a_batch_larger_than_one_pass_is_split_and_keeps_its_order() -> TestResult {
    let directory = tempfile::tempdir()?;
    let encoder = loaded(directory.path())?;
    let texts: Vec<String> = ["load", "config", "read", "search", "index"]
        .iter()
        .map(|word| document(&Declaration::new("fn", word).source(word)).into_text())
        .collect();
    let batched = encoder.embed_documents(&texts)?;
    assert_eq!(batched.len(), texts.len());
    for (index, text) in texts.iter().enumerate() {
        let alone = encoder.embed_documents(std::slice::from_ref(text))?;
        let difference: f32 = batched[index]
            .iter()
            .zip(&alone[0])
            .map(|(one, other)| (one - other).abs())
            .sum();
        assert!(
            difference < 1e-4,
            "batching must not change a vector: {difference} at {index}"
        );
    }
    Ok(())
}

#[test]
fn more_texts_than_the_bound_are_refused_before_any_pass_runs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let encoder = loaded(directory.path())?;
    let texts: Vec<String> = (0..=encoder.limits().texts_max())
        .map(|index| format!("fn item{index}"))
        .collect();
    let error = encoder
        .embed_documents(&texts)
        .expect_err("the bound must refuse the call");
    assert_eq!(error.fault().violation(), SearchViolation::TextLimit);
    assert!(error.to_string().contains("9 texts, 8 allowed"), "{error}");
    Ok(())
}

#[test]
fn an_empty_call_embeds_nothing_and_refuses_nothing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let encoder = loaded(directory.path())?;
    assert!(encoder.embed_documents(&[])?.is_empty());
    Ok(())
}

#[test]
fn a_query_embeds_to_a_unit_vector_and_differs_from_the_bare_text() -> TestResult {
    let directory = tempfile::tempdir()?;
    let encoder = loaded(directory.path())?;
    let query = encoder.embed_query("load config")?;
    assert_eq!(query.len(), HIDDEN);
    let length: f32 = query.iter().map(|value| value * value).sum();
    assert!((length - 1.0).abs() < 1e-4, "got {length}");
    let bare = encoder.embed_documents(&["load config".to_owned()])?;
    let difference: f32 = query
        .iter()
        .zip(&bare[0])
        .map(|(one, other)| (one - other).abs())
        .sum();
    assert!(
        difference > 1e-6,
        "the query prefix must reach the encoder, difference {difference}"
    );
    Ok(())
}

#[test]
fn the_debug_render_names_the_models_shape_without_its_weights() -> TestResult {
    let directory = tempfile::tempdir()?;
    let encoder = loaded(directory.path())?;
    let rendered = format!("{encoder:?}");
    assert!(rendered.starts_with("Encoder"), "{rendered}");
    assert!(rendered.contains("dimension: 8"), "{rendered}");
    assert!(rendered.contains("EncoderLimits"), "{rendered}");
    assert!(
        !rendered.contains("weight"),
        "the weights have no useful rendering: {rendered}"
    );
    Ok(())
}

#[test]
fn a_batch_the_tokenizer_empties_is_refused_rather_than_embedded() -> TestResult {
    let directory = tempfile::tempdir()?;
    write_configuration(directory.path())?;
    write_tokenizer_with(directory.path(), false)?;
    write_weights(directory.path())?;
    let files = ModelFiles::in_directory(directory.path())?;
    let encoder = Encoder::load(&files, limits())?;
    let error = encoder
        .embed_documents(&[String::new()])
        .expect_err("no tokens means no vector");
    assert_eq!(error.fault().violation(), SearchViolation::EncodeFailed);
    assert!(
        error
            .to_string()
            .contains("the tokenizer produced no tokens"),
        "{error}"
    );
    Ok(())
}

/// The document and query handles Rig's contract is called through, over the fixture
/// model this suite already builds.
///
/// The adapters are what every caller of the vector ranking reaches, so the difference
/// the encoder keeps between a document and a query has to survive the trait: a model
/// family added later must not be able to apply one transformation to both sides by
/// accident.
mod through_the_embedding_contract {
    use super::{HIDDEN, TestResult, loaded, write_model};
    use rift_search::{
        BatchSchedule, EmbeddingModels, EmbeddingSpace, LOCAL_INPUTS_MAX, LocalEncoder,
        QueryTransformation, RetrievalModels,
    };
    use rig_core::embeddings::EmbeddingModel as _;
    use std::sync::Arc;

    /// The fixture model behind both handles, in its own space.
    fn pair(directory: &std::path::Path) -> TestResult<EmbeddingModels> {
        write_model(directory)?;
        let held = LocalEncoder::new(Arc::new(loaded(directory)?));
        let space = EmbeddingSpace::local(
            "directory",
            "fixture",
            "fixture",
            HIDDEN,
            QueryTransformation::Instructed("probe-instruction"),
        );
        Ok(EmbeddingModels::Local(RetrievalModels::new(
            held.documents(),
            held.query(),
            space,
        )))
    }

    #[tokio::test]
    async fn the_two_handles_transform_one_text_differently() -> TestResult {
        let directory = tempfile::tempdir()?;
        write_model(directory.path())?;
        let held = LocalEncoder::new(Arc::new(loaded(directory.path())?));
        let documents = held.documents();
        let query = held.query();
        assert_eq!(documents.ndims(), HIDDEN);
        assert_eq!(query.ndims(), HIDDEN);

        let embedded = documents
            .embed_texts(vec!["load config".to_owned()])
            .await?;
        let asked = query.embed_texts(vec!["load config".to_owned()]).await?;
        assert_eq!(embedded.len(), 1);
        assert_eq!(asked.len(), 1);
        assert_eq!(embedded[0].document, "load config");
        let difference: f64 = embedded[0]
            .vec
            .iter()
            .zip(&asked[0].vec)
            .map(|(one, other)| (one - other).abs())
            .sum();
        assert!(
            difference > 1e-6,
            "this checkpoint's query instruction must survive the contract, \
             difference {difference}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_pass_keeps_every_vector_in_its_own_input_slot() -> TestResult {
        let directory = tempfile::tempdir()?;
        let models = pair(directory.path())?;
        let texts: Vec<String> = ["load", "config", "read", "search", "index"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let embedded = models
            .embed_documents(texts.clone(), BatchSchedule::local(2))
            .await?;
        assert_eq!(embedded.len(), texts.len());
        assert!(embedded.iter().all(|vector| vector.len() == HIDDEN));
        let one_at_a_time = models
            .embed_documents(texts, BatchSchedule::local(1))
            .await?;
        assert_eq!(
            embedded, one_at_a_time,
            "the batch bound cuts the pass into requests and never reorders it"
        );
        Ok(())
    }

    #[tokio::test]
    async fn one_query_embeds_through_the_query_handle() -> TestResult {
        let directory = tempfile::tempdir()?;
        let models = pair(directory.path())?;
        let embedded = models.embed_query("load config").await?;
        assert_eq!(embedded.len(), HIDDEN);
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_pass_embeds_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let models = pair(directory.path())?;
        assert!(
            models
                .embed_documents(Vec::new(), BatchSchedule::local(4))
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn a_local_handle_states_the_widest_batch_it_accepts() {
        assert_eq!(
            <rift_search::RiftLocalDocumentModel as rig_core::embeddings::EmbeddingModel>::MAX_DOCUMENTS,
            LOCAL_INPUTS_MAX
        );
        assert_eq!(
            <rift_search::RiftLocalQueryModel as rig_core::embeddings::EmbeddingModel>::MAX_DOCUMENTS,
            LOCAL_INPUTS_MAX
        );
    }
}
