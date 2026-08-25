//! Both tiers behind one index, against a model built in the test, so no suite
//! touches the network.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use candle_core::{DType, Device, Tensor};
use rift_core::ProjectPath;
use rift_index::{
    LexicalIndexLimits, LexicalSearchIndex, LexicalUnit, LexicalUnitKind, SemanticVectorStore,
    StoredVector,
};
use rift_search::{
    AcquisitionLimits, Declaration, DescribedUnit, DocumentDigest, Embedding, ModelSource,
    RankedUnit, RevisionScoped, SearchError, SearchIndex, SearchIndexLimits, SearchViolation,
    SemanticReadiness, document,
};
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

/// A value no pooled vector can hold, so a stored row carrying it proves that
/// row was not embedded again.
const MARK: f32 = 7.0;

/// The tree revision every pass in these suites stamps unless it states
/// another.
const REVISION: &str = "rev-one";

/// A row bound no suite here reaches, so a read answers with every row.
const EVERY: usize = usize::MAX;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Writes one loadable model into `directory`.
fn write_model(directory: &Path) -> TestResult {
    std::fs::create_dir_all(directory)?;
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

/// Deterministic weights: identity-ish layer norms so a pooled vector is
/// finite, and small varying values everywhere else so two texts differ.
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

/// A workspace root holding one loadable model directory.
fn workspace() -> Fallible<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    write_model(&root.path().join("model"))?;
    Ok(root)
}

fn lexical_limits() -> LexicalIndexLimits {
    LexicalIndexLimits::new(64, 1 << 20, 32, 64, 4, 1_000)
}

/// Bounds small enough that every pass and every batch is visible.
fn limits() -> SearchIndexLimits {
    SearchIndexLimits::builder(lexical_limits())
        .batch_declarations(2)
        .max_tokens(16)
        .build()
}

fn database(root: &Path) -> PathBuf {
    root.join("search.db")
}

fn model_source(root: &Path, name: &str) -> Fallible<ModelSource> {
    Ok(ModelSource::directory(name, root)?)
}

/// The identity a directory model's vectors are addressed under.
fn model_identity(root: &Path, name: &str) -> String {
    root.join(name).display().to_string()
}

/// An acquisition that spends no wall clock: a directory model reads no
/// network.
fn acquisition_limits() -> AcquisitionLimits {
    AcquisitionLimits::new(
        Duration::from_secs(1),
        1,
        Duration::from_millis(1),
        Duration::from_millis(1),
    )
}

async fn opened(root: &Path, limits: SearchIndexLimits) -> Fallible<SearchIndex> {
    Ok(SearchIndex::open(&database(root), limits).await?)
}

/// One index with its encoder loaded from the workspace's own model.
async fn prepared(root: &Path, limits: SearchIndexLimits) -> Fallible<SearchIndex> {
    let index = opened(root, limits).await?;
    index
        .prepare(&model_source(root, "model")?, acquisition_limits())
        .await?;
    Ok(index)
}

fn unit(identity: &str, path: &str, name: &str, content: &str) -> Fallible<LexicalUnit> {
    Ok(LexicalUnit::new(
        identity,
        ProjectPath::new(path.to_owned())?,
        LexicalUnitKind::Symbol,
        Some(name.to_owned()),
        content,
    )?)
}

/// The digest one declaration's document is addressed by.
fn digest_of(declaration: &Declaration<'_>) -> String {
    DocumentDigest::of(document(declaration).text()).to_hex()
}

async fn store(root: &Path) -> Fallible<SemanticVectorStore> {
    Ok(SemanticVectorStore::open(&database(root), lexical_limits()).await?)
}

/// The vectors one model holds, in digest order.
async fn stored(root: &Path, name: &str) -> Fallible<Vec<StoredVector>> {
    Ok(store(root)
        .await?
        .vectors(&model_identity(root, name), HIDDEN, EVERY)
        .await?)
}

/// Overwrites one digest's vector with [`MARK`], so a later pass that embedded
/// it again would erase the mark.
async fn mark(root: &Path, digest: &str) -> TestResult {
    store(root)
        .await?
        .store(
            &model_identity(root, "model"),
            HIDDEN,
            &[StoredVector::new(digest.to_owned(), vec![MARK; HIDDEN])],
        )
        .await?;
    Ok(())
}

/// Whether the stored row for `digest` still carries the mark.
fn is_marked(vectors: &[StoredVector], digest: &str) -> bool {
    vectors
        .iter()
        .filter(|stored| stored.digest() == digest)
        .any(|stored| stored.values().iter().all(|value| *value == MARK))
}

/// Deletes every vector one model holds, through a second handle on the same
/// database, so the index under test never learns the rows have gone.
async fn drop_stored_vectors(root: &Path, name: &str) -> TestResult {
    let dropped = store(root)
        .await?
        .prune_absent(&model_identity(root, name), &BTreeSet::new())
        .await?;
    assert!(dropped > 0, "the pass left rows to delete");
    Ok(())
}

/// The order the lexical tier alone puts a query in.
async fn lexical_order(root: &Path, query: &str, limit: u32) -> Fallible<Vec<String>> {
    let index = LexicalSearchIndex::open(&database(root), lexical_limits()).await?;
    let RevisionScoped::Matched(matches) = index.search(REVISION, query, limit).await? else {
        return Err("the lexical store must hold the fixture revision".into());
    };
    Ok(matches
        .iter()
        .map(|matched| matched.identity().to_owned())
        .collect())
}

/// The units one search ranked, refusing an answer the store could not place under
/// `REVISION`.
async fn ranked_units(index: &SearchIndex, query: &str, limit: u32) -> Fallible<Vec<RankedUnit>> {
    match index.search(REVISION, query, limit).await? {
        RevisionScoped::Matched(ranked) => Ok(ranked),
        other => Err(format!("the store must hold {REVISION}: {other:?}").into()),
    }
}

fn identities(ranked: &[RankedUnit]) -> Vec<&str> {
    ranked.iter().map(RankedUnit::identity).collect()
}

fn paths(ranked: &[RankedUnit]) -> Vec<&str> {
    ranked.iter().map(|unit| unit.path().as_str()).collect()
}

/// Each ranked unit as the pair a vector had to land on, in identity order, so
/// an assertion pins the pairing rather than the similarity order.
fn placed(ranked: &[RankedUnit]) -> Vec<(&str, &str)> {
    let mut placed: Vec<(&str, &str)> = ranked
        .iter()
        .map(|unit| (unit.identity(), unit.path().as_str()))
        .collect();
    placed.sort_unstable();
    placed
}

/// Two units in two files, one word apart.
fn two_units() -> Fallible<Vec<LexicalUnit>> {
    Ok(vec![
        unit("one", "src/one.rs", "load_config", "fn load config")?,
        unit("two", "src/two.rs", "read_index", "fn read index")?,
    ])
}

fn two_declarations() -> Vec<Declaration<'static>> {
    vec![
        Declaration::new("fn", "load_config").source("fn load config"),
        Declaration::new("fn", "read_index").source("fn read index"),
    ]
}

/// Each unit paired with the declaration at the same position, for suites
/// whose unit set is symbols alone.
fn described<'a>(
    units: &'a [LexicalUnit],
    declarations: &'a [Declaration<'a>],
) -> Vec<DescribedUnit<'a>> {
    units
        .iter()
        .zip(declarations)
        .map(|(unit, declaration)| DescribedUnit::new(unit, *declaration))
        .collect()
}

/// The two-unit fixture, holding what a pass borrows from.
struct Fixture {
    units: Vec<LexicalUnit>,
    declarations: Vec<Declaration<'static>>,
}

impl Fixture {
    fn units(&self) -> &[LexicalUnit] {
        &self.units
    }

    fn described(&self) -> Vec<DescribedUnit<'_>> {
        described(&self.units, &self.declarations)
    }
}

fn two() -> Fallible<Fixture> {
    Ok(Fixture {
        units: two_units()?,
        declarations: two_declarations(),
    })
}

/// One text file unit, which no declaration describes.
fn text_unit(identity: &str, path: &str, name: &str, content: &str) -> Fallible<LexicalUnit> {
    Ok(LexicalUnit::new(
        identity,
        ProjectPath::new(path.to_owned())?,
        LexicalUnitKind::TextFile,
        Some(name.to_owned()),
        content,
    )?)
}

/// Runs one whole build pass the way the server's population path does: the lexical set is
/// replaced and stamped, then every described declaration is embedded.
async fn whole_pass(
    index: &SearchIndex,
    units: &[LexicalUnit],
    described: &[DescribedUnit<'_>],
    tree_revision: &str,
) -> Result<(), SearchError> {
    index.replace_lexical(units, tree_revision).await?;
    index
        .embed_described(described, Embedding::Every, tree_revision)
        .await
}

/// The same pass incrementally: only declarations the store has no vector for are embedded.
async fn incremental_pass(
    index: &SearchIndex,
    units: &[LexicalUnit],
    described: &[DescribedUnit<'_>],
    tree_revision: &str,
) -> Result<(), SearchError> {
    index.replace_lexical(units, tree_revision).await?;
    index
        .embed_described(described, Embedding::Missing, tree_revision)
        .await
}

#[tokio::test]
async fn a_fresh_path_starts_preparing_and_reopening_reads_what_was_left() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), limits()).await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Preparing {
            prepared: 0,
            total: 0
        }
    );
    assert_eq!(index.tree_revision().await?, None, "nothing has been built");
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    drop(index);

    let reopened = opened(root.path(), limits()).await?;
    assert_eq!(reopened.tree_revision().await?, Some(REVISION.to_owned()));
    assert_eq!(
        identities(&ranked_units(&reopened, "load config", 10).await?),
        ["one"],
        "the units the first index wrote are still there"
    );
    Ok(())
}

#[tokio::test]
async fn a_build_with_no_declarations_leaves_the_semantic_tier_ready() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    whole_pass(&index, &[], &[], REVISION).await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Ready,
        "an empty set has a vector for every declaration it holds"
    );
    assert_eq!(index.tree_revision().await?, Some(REVISION.to_owned()));
    assert!(stored(root.path(), "model").await?.is_empty());
    assert!(
        ranked_units(&index, "load config", 10).await?.is_empty(),
        "a tier holding no vector ranks nothing, and neither tier refuses"
    );
    Ok(())
}

#[tokio::test]
async fn a_build_gives_every_declaration_a_vector_and_stamps_the_tree_revision() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let units = two_units()?;
    let declarations = two_declarations();
    whole_pass(&index, &units, &described(&units, &declarations), REVISION).await?;

    assert_eq!(index.readiness(), SemanticReadiness::Ready);
    assert_eq!(index.tree_revision().await?, Some(REVISION.to_owned()));
    let vectors = stored(root.path(), "model").await?;
    let held: Vec<&str> = vectors.iter().map(StoredVector::digest).collect();
    for declaration in &declarations {
        let digest = digest_of(declaration);
        assert!(
            held.contains(&digest.as_str()),
            "every declaration carries a vector: {digest} missing from {held:?}"
        );
    }
    assert_eq!(vectors.len(), declarations.len());
    Ok(())
}

#[tokio::test]
async fn a_refresh_leaves_a_moved_declaration_the_vector_it_already_had() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let units = two_units()?;
    let declarations = two_declarations();
    whole_pass(&index, &units, &described(&units, &declarations), REVISION).await?;
    let carried = digest_of(&declarations[0]);
    mark(root.path(), &carried).await?;

    let moved = vec![
        unit("moved", "src/moved.rs", "load_config", "fn load config")?,
        unit("two", "src/two.rs", "read_index", "fn read index")?,
    ];
    incremental_pass(&index, &moved, &described(&moved, &declarations), "rev-two").await?;

    let vectors = stored(root.path(), "model").await?;
    assert_eq!(vectors.len(), 2, "a move embeds nothing new");
    assert!(
        is_marked(&vectors, &carried),
        "a declaration whose own text is unchanged keeps the vector it had"
    );
    assert_eq!(index.readiness(), SemanticReadiness::Ready);
    assert_eq!(index.tree_revision().await?, Some("rev-two".to_owned()));
    Ok(())
}

#[tokio::test]
async fn a_refresh_prunes_what_left_and_embeds_what_arrived() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let built = two_units()?;
    let declarations = two_declarations();
    whole_pass(&index, &built, &described(&built, &declarations), REVISION).await?;
    let kept = digest_of(&declarations[0]);
    let removed = digest_of(&declarations[1]);
    mark(root.path(), &kept).await?;

    let units = vec![
        unit("one", "src/one.rs", "load_config", "fn load config")?,
        unit("three", "src/three.rs", "search_index", "fn search index")?,
    ];
    let arrived = Declaration::new("fn", "search_index").source("fn search index");
    let declarations = vec![declarations[0], arrived];
    incremental_pass(&index, &units, &described(&units, &declarations), REVISION).await?;

    let vectors = stored(root.path(), "model").await?;
    let held: Vec<&str> = vectors.iter().map(StoredVector::digest).collect();
    assert!(
        !held.contains(&removed.as_str()),
        "a removed declaration's vector is pruned: {held:?}"
    );
    assert!(
        held.contains(&digest_of(&arrived).as_str()),
        "an added declaration is embedded: {held:?}"
    );
    assert!(
        is_marked(&vectors, &kept),
        "the declaration that stayed was not embedded again"
    );
    Ok(())
}

#[tokio::test]
async fn a_full_pass_embeds_a_declaration_the_store_already_holds() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let units = two_units()?;
    let declarations = two_declarations();
    let described = described(&units, &declarations);
    whole_pass(&index, &units, &described, REVISION).await?;
    let carried = digest_of(&declarations[0]);
    mark(root.path(), &carried).await?;

    whole_pass(&index, &units, &described, REVISION).await?;
    assert!(
        !is_marked(&stored(root.path(), "model").await?, &carried),
        "a build does not trust what is stored, so the mark is overwritten"
    );
    Ok(())
}

#[tokio::test]
async fn a_build_stopping_at_the_vector_bound_reports_preparing() -> TestResult {
    let root = workspace()?;
    let bounded = SearchIndexLimits::builder(lexical_limits())
        .batch_declarations(2)
        .max_tokens(16)
        .max_vectors(1)
        .build();
    let index = prepared(root.path(), bounded).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Preparing {
            prepared: 1,
            total: 2
        },
        "stopping at the bound is reported, not swallowed"
    );
    assert_eq!(stored(root.path(), "model").await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_disabled_tier_answers_in_the_lexical_order_alone() -> TestResult {
    let root = workspace()?;
    let disabled = SearchIndexLimits::builder(lexical_limits())
        .disable_semantic()
        .build();
    let index = opened(root.path(), disabled).await?;
    assert_eq!(index.readiness(), SemanticReadiness::Disabled);
    index
        .prepare(&model_source(root.path(), "model")?, acquisition_limits())
        .await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Disabled,
        "a disabled tier acquires nothing"
    );
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(index.readiness(), SemanticReadiness::Disabled);
    assert!(
        stored(root.path(), "model").await?.is_empty(),
        "a disabled tier embeds nothing"
    );

    let ranked = ranked_units(&index, "load config read index", 10).await?;
    assert_eq!(
        identities(&ranked),
        lexical_order(root.path(), "load config read index", 10).await?,
        "with one ranking the fused order is the lexical order"
    );
    assert_eq!(ranked[0].kind(), LexicalUnitKind::Symbol);
    assert!(ranked[0].score() > 0.0);
    Ok(())
}

/// The fixture model's vectors carry no trained meaning: its two layers hold
/// values this suite wrote, so nothing here measures relevance. What it proves
/// is that the semantic ranking reaches the fused result at all, for a query
/// the lexical tier cannot answer.
#[tokio::test]
async fn a_query_the_lexical_tier_cannot_answer_is_still_ranked_through_the_semantic_tier()
-> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;

    assert!(
        lexical_order(root.path(), "search", 10).await?.is_empty(),
        "the query shares no token with either unit"
    );
    let ranked = ranked_units(&index, "search", 10).await?;
    let reached = identities(&ranked);
    assert!(
        reached.contains(&"one") && reached.contains(&"two"),
        "the semantic tier reaches units the lexical tier cannot: {reached:?}"
    );
    assert_eq!(paths(&ranked).len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_vector_lands_on_the_unit_whose_declaration_produced_it() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let units = vec![
        text_unit("doc-one", "docs/one.md", "one", "notes about loading")?,
        unit("sym-one", "src/one.rs", "load_config", "fn load config")?,
        text_unit("doc-two", "docs/two.md", "two", "notes about reading")?,
        unit("sym-two", "src/two.rs", "read_index", "fn read index")?,
    ];
    let declarations = two_declarations();
    let described = vec![
        DescribedUnit::new(&units[1], declarations[0]),
        DescribedUnit::new(&units[3], declarations[1]),
    ];
    whole_pass(&index, &units, &described, REVISION).await?;

    assert_eq!(stored(root.path(), "model").await?.len(), 2);
    assert!(lexical_order(root.path(), "search", 10).await?.is_empty());
    let ranked = ranked_units(&index, "search", 10).await?;
    let reached = placed(&ranked);
    assert_eq!(
        reached,
        [("sym-one", "src/one.rs"), ("sym-two", "src/two.rs")],
        "pairing by position would have put these vectors on doc-one and sym-one: {reached:?}"
    );
    for one in &ranked {
        assert_eq!(one.kind(), LexicalUnitKind::Symbol);
    }
    Ok(())
}

#[tokio::test]
async fn more_units_than_described_entries_leave_the_undescribed_ones_without_a_vector()
-> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let units = vec![
        unit("sym-one", "src/one.rs", "load_config", "fn load config")?,
        text_unit("doc-one", "docs/one.md", "one", "notes about loading")?,
        text_unit("doc-two", "docs/two.md", "two", "notes about reading")?,
    ];
    let declarations = two_declarations();
    let described = vec![DescribedUnit::new(&units[0], declarations[0])];
    whole_pass(&index, &units, &described, REVISION).await?;

    let vectors = stored(root.path(), "model").await?;
    assert_eq!(
        vectors.len(),
        1,
        "a unit no declaration describes is not embedded"
    );
    assert_eq!(vectors[0].digest(), digest_of(&declarations[0]));
    assert_eq!(
        identities(&ranked_units(&index, "search", 10).await?),
        ["sym-one"],
        "the only vector resolves to the only described unit"
    );
    assert_eq!(index.readiness(), SemanticReadiness::Ready);
    Ok(())
}

#[tokio::test]
async fn more_described_entries_than_units_still_land_each_vector_on_its_own_unit() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let indexed = vec![unit("one", "src/one.rs", "load_config", "fn load config")?];
    let apart = unit("two", "src/two.rs", "read_index", "fn read index")?;
    let declarations = two_declarations();
    let described = vec![
        DescribedUnit::new(&indexed[0], declarations[0]),
        DescribedUnit::new(&apart, declarations[1]),
    ];
    whole_pass(&index, &indexed, &described, REVISION).await?;

    let vectors = stored(root.path(), "model").await?;
    let held: Vec<&str> = vectors.iter().map(StoredVector::digest).collect();
    assert_eq!(held.len(), 2, "every described unit is embedded: {held:?}");
    let ranked = ranked_units(&index, "search", 10).await?;
    assert_eq!(
        placed(&ranked),
        [("one", "src/one.rs"), ("two", "src/two.rs")],
        "a described unit the lexical set never held still ranks under its own address"
    );
    Ok(())
}

#[tokio::test]
async fn a_tier_that_will_not_load_leaves_the_lexical_ranking_serving() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Preparing {
            prepared: 0,
            total: 2
        },
        "a pass with no encoder embeds nothing and says so"
    );

    let error = index
        .prepare(&model_source(root.path(), "absent")?, acquisition_limits())
        .await
        .expect_err("the directory holds no model");
    assert_eq!(error.fault().violation(), SearchViolation::ModelFileMissing);
    assert_eq!(index.readiness(), SemanticReadiness::Unavailable);

    assert_eq!(
        identities(&ranked_units(&index, "load config", 10).await?),
        ["one"],
        "the lexical tier keeps answering"
    );
    assert!(
        stored(root.path(), "model").await?.is_empty(),
        "an unavailable tier embeds nothing"
    );
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Unavailable,
        "one failure is final for the life of this index"
    );
    Ok(())
}

#[tokio::test]
async fn an_empty_query_answers_nothing_and_a_limit_of_one_answers_once() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;

    assert!(ranked_units(&index, "", 10).await?.is_empty());
    assert!(
        ranked_units(&index, "   ", 10).await?.is_empty(),
        "a query of blanks has no term either"
    );
    assert_eq!(ranked_units(&index, "load config", 1).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn one_files_declarations_cannot_fill_the_candidate_list() -> TestResult {
    let root = workspace()?;
    let spread = SearchIndexLimits::builder(lexical_limits())
        .batch_declarations(2)
        .max_tokens(16)
        .candidates(4)
        .per_file_max(1)
        .build();
    let index = prepared(root.path(), spread).await?;
    let units = vec![
        unit("crowd-one", "src/crowd.rs", "load", "fn load")?,
        unit("crowd-two", "src/crowd.rs", "config", "fn config")?,
        unit("crowd-three", "src/crowd.rs", "read", "fn read")?,
        unit("lone", "src/lone.rs", "index", "fn index")?,
    ];
    let declarations = vec![
        Declaration::new("fn", "load").source("fn load"),
        Declaration::new("fn", "config").source("fn config"),
        Declaration::new("fn", "read").source("fn read"),
        Declaration::new("fn", "index").source("fn index"),
    ];
    whole_pass(&index, &units, &described(&units, &declarations), REVISION).await?;

    assert!(lexical_order(root.path(), "search", 10).await?.is_empty());
    let ranked = ranked_units(&index, "search", 10).await?;
    let reached = paths(&ranked);
    assert_eq!(
        reached.len(),
        2,
        "one file contributes one candidate: {reached:?}"
    );
    assert!(
        reached.contains(&"src/lone.rs"),
        "the crowded file cannot push the other one out: {reached:?}"
    );
    Ok(())
}

#[tokio::test]
async fn readiness_walks_from_preparing_to_ready() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), limits()).await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Preparing {
            prepared: 0,
            total: 0
        }
    );
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.readiness(),
        SemanticReadiness::Preparing {
            prepared: 0,
            total: 2
        }
    );
    index
        .prepare(&model_source(root.path(), "model")?, acquisition_limits())
        .await?;
    incremental_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(index.readiness(), SemanticReadiness::Ready);
    Ok(())
}

#[tokio::test]
async fn vectors_with_no_unit_to_rank_them_as_leave_the_lexical_ranking_alone() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    drop(index);

    let reopened = prepared(root.path(), limits()).await?;
    assert_eq!(
        stored(root.path(), "model").await?.len(),
        2,
        "the vectors the first index wrote are still stored"
    );
    assert!(
        ranked_units(&reopened, "search", 10).await?.is_empty(),
        "no pass has said which unit each digest belongs to"
    );
    assert_eq!(
        identities(&ranked_units(&reopened, "load config", 10).await?),
        ["one"],
        "the lexical tier answers on its own"
    );
    Ok(())
}

#[tokio::test]
async fn a_model_change_drops_the_vectors_the_previous_model_wrote() -> TestResult {
    let root = workspace()?;
    write_model(&root.path().join("other"))?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(stored(root.path(), "model").await?.len(), 2);
    drop(index);

    let changed = opened(root.path(), limits()).await?;
    changed
        .prepare(&model_source(root.path(), "other")?, acquisition_limits())
        .await?;
    assert!(
        stored(root.path(), "model").await?.is_empty(),
        "two models address different spaces, so the previous rows can never be read"
    );
    whole_pass(&changed, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(stored(root.path(), "other").await?.len(), 2);
    Ok(())
}

/// A query reads no vector row. What proves it is the store: the rows one pass
/// wrote are deleted underneath the index, and the same query answers with the
/// same units afterwards, so what it ranked was the corpus that pass published.
#[tokio::test]
async fn a_query_ranks_from_the_held_corpus_after_the_stored_rows_are_gone() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert!(
        lexical_order(root.path(), "search", 10).await?.is_empty(),
        "the query shares no token with either unit, so only the semantic tier can answer it"
    );
    let answered = ranked_units(&index, "search", 10).await?;
    let ranked = placed(&answered);
    assert_eq!(ranked, [("one", "src/one.rs"), ("two", "src/two.rs")]);

    drop_stored_vectors(root.path(), "model").await?;
    assert!(
        stored(root.path(), "model").await?.is_empty(),
        "no vector row is left for a query to read"
    );
    assert_eq!(
        placed(&ranked_units(&index, "search", 10).await?),
        ranked,
        "the ranking still answers, so the query path never read the vector table"
    );
    Ok(())
}

/// A refresh embeds only what the store was missing, so the corpus it
/// publishes has to come from the store rather than from what it embedded.
#[tokio::test]
async fn a_refresh_publishes_what_it_embedded_beside_what_the_build_left() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let built = vec![unit("one", "src/one.rs", "load_config", "fn load config")?];
    let carried = Declaration::new("fn", "load_config").source("fn load config");
    whole_pass(&index, &built, &described(&built, &[carried]), REVISION).await?;

    let units = vec![
        unit("one", "src/one.rs", "load_config", "fn load config")?,
        unit("three", "src/three.rs", "read_index", "fn read index")?,
    ];
    let arrived = Declaration::new("fn", "read_index").source("fn read index");
    incremental_pass(
        &index,
        &units,
        &described(&units, &[carried, arrived]),
        REVISION,
    )
    .await?;
    assert_eq!(
        stored(root.path(), "model").await?.len(),
        2,
        "the refresh embedded the declaration that arrived and kept the one that stayed"
    );

    drop_stored_vectors(root.path(), "model").await?;
    assert!(lexical_order(root.path(), "search", 10).await?.is_empty());
    assert_eq!(
        placed(&ranked_units(&index, "search", 10).await?),
        [("one", "src/one.rs"), ("three", "src/three.rs")],
        "the corpus the refresh published holds the vector it never embedded itself"
    );
    Ok(())
}

/// Two models address different spaces, so a query this encoder embedded may
/// never be scored against the previous encoder's vectors.
#[tokio::test]
async fn a_model_change_leaves_none_of_the_previous_models_vectors_held() -> TestResult {
    let root = workspace()?;
    write_model(&root.path().join("other"))?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(
        placed(&ranked_units(&index, "search", 10).await?),
        [("one", "src/one.rs"), ("two", "src/two.rs")]
    );

    index
        .prepare(&model_source(root.path(), "other")?, acquisition_limits())
        .await?;
    assert!(
        ranked_units(&index, "search", 10).await?.is_empty(),
        "the corpus the previous model filled is held nowhere, and the lexical tier cannot answer this query"
    );
    assert_eq!(
        identities(&ranked_units(&index, "load config", 10).await?),
        ["one"],
        "the lexical tier keeps answering"
    );

    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(
        placed(&ranked_units(&index, "search", 10).await?),
        [("one", "src/one.rs"), ("two", "src/two.rs")],
        "a pass under the model now held publishes a corpus of its own"
    );
    Ok(())
}

/// What the index holds between passes is bounded by the same ceiling the pass
/// cuts the described set to, so the memory one index spends on vectors is the
/// number the operator set and not what the file happens to hold.
#[tokio::test]
async fn the_held_corpus_stops_at_the_vector_bound() -> TestResult {
    let root = workspace()?;
    let bounded = SearchIndexLimits::builder(lexical_limits())
        .batch_declarations(2)
        .max_tokens(16)
        .max_vectors(1)
        .build();
    let index = prepared(root.path(), bounded).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert_eq!(stored(root.path(), "model").await?.len(), 1);

    drop_stored_vectors(root.path(), "model").await?;
    assert!(lexical_order(root.path(), "search", 10).await?.is_empty());
    assert_eq!(
        identities(&ranked_units(&index, "search", 10).await?),
        ["one"],
        "the corpus carries the one vector the bound left room for, and no more"
    );
    Ok(())
}

#[tokio::test]
async fn a_store_refusal_carries_the_stores_own_violation() -> TestResult {
    let root = workspace()?;
    let narrow = SearchIndexLimits::builder(LexicalIndexLimits::new(1, 1 << 20, 32, 64, 4, 1_000))
        .disable_semantic()
        .build();
    let index = opened(root.path(), narrow).await?;
    let fixture = two()?;
    let error = whole_pass(&index, fixture.units(), &fixture.described(), REVISION)
        .await
        .expect_err("two units pass the one-unit bound");
    assert_eq!(error.fault().violation(), SearchViolation::StoreFailed);
    let rendered = error.to_string();
    assert!(rendered.contains("store_failed"), "{rendered}");
    assert!(
        rendered.contains("unit_limit"),
        "the lexical tier's own violation rides along: {rendered}"
    );
    assert!(std::error::Error::source(&error).is_some());
    Ok(())
}

/// A bound the store enforced reaches the caller as that bound.
///
/// Flattening every store refusal into this tier's own violation told a caller
/// sending too many query terms that the server had failed, when the caller
/// could have shortened the query. The registry identity and the limit
/// evidence travel with the failure so the answer stays actionable.
#[tokio::test]
async fn a_store_bound_keeps_its_registry_identity_and_its_limit_evidence() -> TestResult {
    let root = workspace()?;
    let narrow = SearchIndexLimits::builder(LexicalIndexLimits::new(1, 1 << 20, 32, 64, 4, 1_000))
        .disable_semantic()
        .build();
    let index = opened(root.path(), narrow).await?;
    let fixture = two()?;
    let error = whole_pass(&index, fixture.units(), &fixture.described(), REVISION)
        .await
        .expect_err("two units pass the one-unit bound");

    let descriptor = error.descriptor();
    assert_eq!(
        descriptor.code(),
        "limit_exceeded",
        "the store's own classification reaches the caller, not this tier's"
    );
    let evidence = rift_core::Fault::limit_evidence(error.fault())
        .expect("a limit refusal states the bound and what the request needed");
    assert_eq!(evidence.field, "units_max");
    assert_eq!(evidence.limit, 1);
    assert_eq!(evidence.required, 2);
    Ok(())
}

#[tokio::test]
async fn opening_a_store_that_cannot_be_created_is_refused() -> TestResult {
    let root = tempfile::tempdir()?;
    let error = SearchIndex::open(root.path(), limits())
        .await
        .expect_err("a directory is not a database file");
    assert_eq!(error.fault().violation(), SearchViolation::StoreFailed);
    Ok(())
}

/// A query for a tree the store has moved past reads no row: the caller's publication was
/// superseded, and the answer it asked for is under a publication it has yet to capture.
#[tokio::test]
async fn a_search_for_a_tree_the_store_moved_past_names_the_stored_revision() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;

    assert_eq!(
        index.search("another-revision", "load config", 10).await?,
        RevisionScoped::OtherRevision(REVISION.to_owned())
    );
    Ok(())
}

/// A store no pass has ever stamped answers for no tree at all, which is not the same as a
/// store holding another one: nothing has landed in it yet.
#[tokio::test]
async fn a_search_before_any_population_reports_no_revision() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;

    assert_eq!(
        index.search(REVISION, "load config", 10).await?,
        RevisionScoped::NoRevision
    );
    Ok(())
}

/// Embedding runs after publication, so a newly published tree meets a corpus described
/// for the previous one. That corpus ranks nothing: the lexical tier answers alone until
/// the pass for this tree lands.
#[tokio::test]
async fn a_corpus_described_for_the_previous_tree_ranks_nothing() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.units(), &fixture.described(), REVISION).await?;
    assert!(
        lexical_order(root.path(), "search", 10).await?.is_empty(),
        "the query shares no token with either unit, so only the semantic tier can answer it"
    );
    assert_eq!(ranked_units(&index, "search", 10).await?.len(), 2);

    // The lexical half of the next publication lands first, as the publication path runs it.
    index.replace_lexical(fixture.units(), "rev-two").await?;
    let RevisionScoped::Matched(ranked) = index.search("rev-two", "search", 10).await? else {
        return Err("the store holds the tree that was just stamped".into());
    };
    assert!(
        ranked.is_empty(),
        "the previous tree's vectors must not rank a tree they were not described for"
    );

    index
        .embed_described(&fixture.described(), Embedding::Missing, "rev-two")
        .await?;
    let RevisionScoped::Matched(ranked) = index.search("rev-two", "search", 10).await? else {
        return Err("the store still holds the tree that was stamped".into());
    };
    assert_eq!(
        ranked.len(),
        2,
        "the pass for this tree publishes a corpus that ranks it"
    );
    Ok(())
}
