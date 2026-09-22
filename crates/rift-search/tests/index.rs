//! Both tiers behind one index, against a model built in the test, so no suite
//! touches the network.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use candle_core::{DType, Device, Tensor};
use rift_core::ProjectPath;
use rift_index::{DatabasePool, WorkspaceDatabase};
use rift_index::{LexicalIndexLimits, LexicalSearchIndex, StoredVector, VectorStore};
use rift_ranking::{
    DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, FieldSet, IndexDocument,
    ParsedQuery, QueryPhase, RankingInput, RankingInputKind, SearchableField,
};
use rift_search::{
    AcquisitionLimits, Declaration, DescribedUnit, DocumentDigest, Embedding, Encoder,
    EncoderLimits, ModelFiles, ModelSource, RevisionScoped, SearchError, SearchIndex,
    SearchIndexLimits, SearchViolation, StoreRanking, VectorReadiness, document,
};
use tokenizers::models::wordpiece::WordPiece;
use tokenizers::processors::bert::BertProcessing;
use tokenizers::{Tokenizer, normalizers, pre_tokenizers};

/// The pooled-connection bounds every suite here opens the database with.
fn database_pool() -> DatabasePool {
    DatabasePool::new(4, 1_000)
}

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
    LexicalIndexLimits::new(64, 1 << 20, 32, 4, 1_000)
}

/// Bounds small enough that every pass and every batch is visible.
fn limits() -> SearchIndexLimits {
    SearchIndexLimits::builder(lexical_limits())
        .batch_declarations(2)
        .max_tokens(16)
        .build()
}

/// The same bounds with the vector ranking turned off, for suites that ask
/// only what the full-text input answered.
fn lexical_only_limits() -> SearchIndexLimits {
    SearchIndexLimits::builder(lexical_limits())
        .disable_vector()
        .build()
}

fn database(root: &Path) -> PathBuf {
    root.join("search.db")
}

fn model_source(root: &Path, name: &str) -> Fallible<ModelSource> {
    Ok(ModelSource::directory(name, root)?)
}

/// The identity a directory model's vectors are addressed under: the same
/// value the index derives, asked of the crate rather than spelled twice.
///
/// The encoder is loaded rather than described, because the width and the
/// query transformation the space records are the loaded model's own answers.
fn model_identity(root: &Path, name: &str) -> Fallible<String> {
    let directory = root.join(name);
    let files = ModelFiles::in_directory(&directory)?;
    let encoder = Encoder::load(&files, EncoderLimits::new(2, 16, 256))?;
    Ok(rift_search::local_embedding_space(
        &ModelSource::Directory(directory),
        &files,
        encoder.dimension(),
        encoder.query_transformation(),
    )
    .identity())
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

/// Builds one project document from an explicit field set, so a suite can
/// state which column carries the term it searches for.
fn project_document(
    identity: &str,
    path: &str,
    kind: DocumentKind,
    fields: DocumentFields,
) -> Fallible<IndexDocument> {
    let digest = fields.digest();
    Ok(IndexDocument::new(
        DocumentIdentity::new(identity)?,
        DocumentLocation::Project(ProjectPath::new(path)?),
        kind,
        digest,
        fields,
    )?)
}

/// One symbol document carrying a declaration name and its own source.
fn symbol(
    identity: &str,
    path: &str,
    name: &str,
    declaration_source: &str,
) -> Fallible<IndexDocument> {
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, name)
        .with(SearchableField::DeclarationSource, declaration_source);
    project_document(identity, path, DocumentKind::Symbol, fields)
}

/// One text-file document under an explicit identity: the final path segment
/// with its extension in `name`, the text in `file_content`. A text file
/// declares nothing, so every declaration field stays absent.
fn text_chunk(identity: &str, path: &str, content: &str) -> Fallible<IndexDocument> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, name)
        .with(SearchableField::FileContent, content);
    project_document(identity, path, DocumentKind::TextFile, fields)
}

/// One whole text-file document; its identity is its own path, per convention.
fn text_document(path: &str, content: &str) -> Fallible<IndexDocument> {
    text_chunk(path, path, content)
}

/// The digest one declaration's document is addressed by.
fn digest_of(declaration: &Declaration<'_>) -> String {
    DocumentDigest::of(document(declaration).text()).to_hex()
}

async fn store(root: &Path) -> Fallible<VectorStore> {
    let database = WorkspaceDatabase::open(&database(root), database_pool()).await?;
    Ok(VectorStore::attached(database))
}

/// The vectors one model holds, in digest order.
async fn stored(root: &Path, name: &str) -> Fallible<Vec<StoredVector>> {
    Ok(store(root)
        .await?
        .vectors(&model_identity(root, name)?, HIDDEN, EVERY)
        .await?)
}

/// Overwrites one digest's vector with [`MARK`], so a later pass that embedded
/// it again would erase the mark.
async fn mark(root: &Path, digest: &str) -> TestResult {
    store(root)
        .await?
        .store(
            &model_identity(root, "model")?,
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
        .prune_absent(&model_identity(root, name)?, &BTreeSet::new())
        .await?;
    assert!(dropped > 0, "the pass left rows to delete");
    Ok(())
}

/// The order the full-text tier alone puts a query in, read through a second
/// handle on the same database.
async fn lexical_order(root: &Path, query: &str, limit: u32) -> Fallible<Vec<String>> {
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database(root), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    let parsed = ParsedQuery::parse(query)?;
    let RevisionScoped::Matched(ranking) = index
        .search(REVISION, &parsed, QueryPhase::Precise, limit)
        .await?
    else {
        return Err("the lexical store must hold the fixture revision".into());
    };
    Ok(ranking
        .matches()
        .iter()
        .map(|matched| matched.identity().as_str().to_owned())
        .collect())
}

/// The ranking one phase produced for `tree_revision`, refusing an answer the
/// store could not place under it.
async fn revision_ranking(
    index: &SearchIndex,
    tree_revision: &str,
    query: &str,
    phase: QueryPhase,
    limit: u32,
) -> Fallible<StoreRanking> {
    let parsed = ParsedQuery::parse(query)?;
    match index.rank(tree_revision, &parsed, phase, limit).await? {
        RevisionScoped::Matched(ranking) => Ok(ranking),
        other => Err(format!("the store must hold {tree_revision}: {other:?}").into()),
    }
}

/// The ranking one phase produced under the fixture revision.
async fn phase_ranking(
    index: &SearchIndex,
    query: &str,
    phase: QueryPhase,
    limit: u32,
) -> Fallible<StoreRanking> {
    revision_ranking(index, REVISION, query, phase, limit).await
}

/// The precise-phase ranking, which is the phase every reader runs first.
async fn ranked(index: &SearchIndex, query: &str, limit: u32) -> Fallible<StoreRanking> {
    phase_ranking(index, query, QueryPhase::Precise, limit).await
}

/// The input `kind` contributed, refusing a ranking that ran without it.
fn input(ranking: &StoreRanking, kind: RankingInputKind) -> Fallible<&RankingInput> {
    let found = ranking.inputs().iter().find(|input| input.kind() == kind);
    Ok(found.ok_or_else(|| format!("the {} input must be present: {ranking:?}", kind.label()))?)
}

/// What produced each input, in the order the store ran them.
fn kinds(ranking: &StoreRanking) -> Vec<RankingInputKind> {
    ranking.inputs().iter().map(RankingInput::kind).collect()
}

/// The identities one input ranked, best first.
fn identities(input: &RankingInput) -> Vec<&str> {
    input
        .order()
        .iter()
        .map(|ranked| ranked.identity().as_str())
        .collect()
}

/// The identities one input ranked, in identity order, so an assertion pins
/// the set a tier reached rather than the order it reached them in.
fn reached(input: &RankingInput) -> Vec<&str> {
    let mut reached = identities(input);
    reached.sort_unstable();
    reached
}

/// The identities the full-text input ranked for `query`, best first.
async fn lexical_identities(index: &SearchIndex, query: &str, limit: u32) -> Fallible<Vec<String>> {
    let ranking = ranked(index, query, limit).await?;
    let carried = input(&ranking, RankingInputKind::Lexical)?;
    Ok(identities(carried).into_iter().map(str::to_owned).collect())
}

/// The identities the vector input ranked for `query`, in identity order.
async fn vector_reached(index: &SearchIndex, query: &str, limit: u32) -> Fallible<Vec<String>> {
    let ranking = ranked(index, query, limit).await?;
    let carried = input(&ranking, RankingInputKind::Vector)?;
    Ok(reached(carried).into_iter().map(str::to_owned).collect())
}

/// The columns that placed one identity in an input.
fn fields_of(carried: &RankingInput, identity: &str) -> Fallible<FieldSet> {
    let found = carried
        .order()
        .iter()
        .find(|ranked| ranked.identity().as_str() == identity);
    Ok(found
        .ok_or_else(|| format!("{identity} must be ranked: {carried:?}"))?
        .fields())
}

/// Two symbol documents in two files, one word apart.
fn two_documents() -> Fallible<Vec<IndexDocument>> {
    Ok(vec![
        symbol("one", "src/one.rs", "load_config", "fn load config")?,
        symbol("two", "src/two.rs", "read_index", "fn read index")?,
    ])
}

fn two_declarations() -> Vec<Declaration<'static>> {
    vec![
        Declaration::new("fn", "load_config").source("fn load config"),
        Declaration::new("fn", "read_index").source("fn read index"),
    ]
}

/// Each document paired with the declaration at the same position, for suites
/// whose document set is symbols alone.
fn described<'a>(
    documents: &'a [IndexDocument],
    declarations: &'a [Declaration<'a>],
) -> Vec<DescribedUnit<'a>> {
    documents
        .iter()
        .zip(declarations)
        .map(|(document, declaration)| DescribedUnit::new(document, *declaration))
        .collect()
}

/// The two-document fixture, holding what a pass borrows from.
struct Fixture {
    documents: Vec<IndexDocument>,
    declarations: Vec<Declaration<'static>>,
}

impl Fixture {
    fn documents(&self) -> &[IndexDocument] {
        &self.documents
    }

    fn described(&self) -> Vec<DescribedUnit<'_>> {
        described(&self.documents, &self.declarations)
    }
}

fn two() -> Fallible<Fixture> {
    Ok(Fixture {
        documents: two_documents()?,
        declarations: two_declarations(),
    })
}

/// Runs one whole build pass the way the server's population path does: the lexical set is
/// replaced and stamped, then every described declaration is embedded.
async fn whole_pass(
    index: &SearchIndex,
    documents: &[IndexDocument],
    described: &[DescribedUnit<'_>],
    tree_revision: &str,
) -> Result<(), SearchError> {
    index.replace_lexical(documents, tree_revision).await?;
    index
        .embed_described(described, Embedding::Every, tree_revision)
        .await
}

/// The same pass incrementally: only declarations the store has no vector for are embedded.
async fn incremental_pass(
    index: &SearchIndex,
    documents: &[IndexDocument],
    described: &[DescribedUnit<'_>],
    tree_revision: &str,
) -> Result<(), SearchError> {
    index.replace_lexical(documents, tree_revision).await?;
    index
        .embed_described(described, Embedding::Missing, tree_revision)
        .await
}

#[tokio::test]
async fn a_fresh_path_starts_preparing_and_reopening_reads_what_was_left() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), limits()).await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Preparing {
            prepared: 0,
            total: 0
        }
    );
    assert_eq!(index.tree_revision().await?, None, "nothing has been built");
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    drop(index);

    let reopened = opened(root.path(), limits()).await?;
    assert_eq!(reopened.tree_revision().await?, Some(REVISION.to_owned()));
    assert_eq!(
        lexical_identities(&reopened, "load config", 10).await?,
        ["one"],
        "the documents the first index wrote are still there"
    );
    Ok(())
}

#[tokio::test]
async fn a_build_with_no_declarations_leaves_the_vector_ranking_ready() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    whole_pass(&index, &[], &[], REVISION).await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Ready,
        "an empty set has a vector for every declaration it holds"
    );
    assert_eq!(index.tree_revision().await?, Some(REVISION.to_owned()));
    assert!(stored(root.path(), "model").await?.is_empty());
    let ranking = ranked(&index, "load config", 10).await?;
    assert!(
        identities(input(&ranking, RankingInputKind::Lexical)?).is_empty(),
        "a tier holding no document ranks nothing, and neither tier refuses"
    );
    assert!(
        identities(input(&ranking, RankingInputKind::Vector)?).is_empty(),
        "a tier holding no vector ranks nothing either"
    );
    assert_eq!(
        ranking.readiness(),
        VectorReadiness::Ready,
        "an empty corpus is every vector an empty set owes, so the read waits \
         for nothing"
    );
    Ok(())
}

#[tokio::test]
async fn a_ranking_carries_the_readiness_its_own_vector_scan_decided() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    index.replace_lexical(fixture.documents(), REVISION).await?;

    // No pass has published a corpus, so the vector tier ranks nothing and the
    // ranking says so.
    let waiting = ranked(&index, "load config", 10).await?;
    assert!(
        identities(input(&waiting, RankingInputKind::Vector)?).is_empty(),
        "a tier with no corpus to scan ranks nothing: {waiting:?}"
    );
    assert_eq!(
        waiting.readiness(),
        VectorReadiness::Preparing {
            prepared: 0,
            total: 0
        },
        "the ranking carries the wait the vector tier is owed"
    );

    // The pass lands. A caller reading the index now reads `Ready`, and the
    // answer it would attach that to is the one above, which ranked nothing.
    index
        .embed_described(&fixture.described(), Embedding::Every, REVISION)
        .await?;
    assert_eq!(index.pass_readiness(), VectorReadiness::Ready);
    assert_eq!(
        waiting.readiness(),
        VectorReadiness::Preparing {
            prepared: 0,
            total: 0
        },
        "a pass landing after a ranking cannot turn that ranking's decline into \
         an answer with nothing left to wait for"
    );

    let answered = ranked(&index, "load config", 10).await?;
    assert_eq!(answered.readiness(), VectorReadiness::Ready);
    assert!(
        !identities(input(&answered, RankingInputKind::Vector)?).is_empty(),
        "the pass published a corpus for this tree, so the tier ranks: {answered:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_corpus_described_for_another_tree_reports_a_wait_rather_than_ready() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(index.pass_readiness(), VectorReadiness::Ready);

    // The lexical lane stamps the next tree as it commits, and the pass that
    // describes that tree runs afterwards. Between the two the corpus answers
    // the previous tree alone.
    index
        .replace_lexical(fixture.documents(), "rev-two")
        .await?;

    let ranking =
        revision_ranking(&index, "rev-two", "load config", QueryPhase::Precise, 10).await?;
    assert!(
        identities(input(&ranking, RankingInputKind::Vector)?).is_empty(),
        "a corpus described for another tree ranks nothing: {ranking:?}"
    );
    assert_eq!(
        ranking.readiness(),
        VectorReadiness::Preparing {
            prepared: 0,
            total: 2
        },
        "the tree this read captured carries no vector yet, whatever the pass \
         reached for the one before it"
    );
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Ready,
        "the pass is finished with the tree it described, which is what a read \
         must not be told about the tree it captured"
    );
    Ok(())
}

#[tokio::test]
async fn a_build_gives_every_declaration_a_vector_and_stamps_the_tree_revision() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let documents = two_documents()?;
    let declarations = two_declarations();
    whole_pass(
        &index,
        &documents,
        &described(&documents, &declarations),
        REVISION,
    )
    .await?;

    assert_eq!(index.pass_readiness(), VectorReadiness::Ready);
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
    let documents = two_documents()?;
    let declarations = two_declarations();
    whole_pass(
        &index,
        &documents,
        &described(&documents, &declarations),
        REVISION,
    )
    .await?;
    let carried = digest_of(&declarations[0]);
    mark(root.path(), &carried).await?;

    let moved = vec![
        symbol("moved", "src/moved.rs", "load_config", "fn load config")?,
        symbol("two", "src/two.rs", "read_index", "fn read index")?,
    ];
    incremental_pass(&index, &moved, &described(&moved, &declarations), "rev-two").await?;

    let vectors = stored(root.path(), "model").await?;
    assert_eq!(vectors.len(), 2, "a move embeds nothing new");
    assert!(
        is_marked(&vectors, &carried),
        "a declaration whose own text is unchanged keeps the vector it had"
    );
    assert_eq!(index.pass_readiness(), VectorReadiness::Ready);
    assert_eq!(index.tree_revision().await?, Some("rev-two".to_owned()));
    Ok(())
}

#[tokio::test]
async fn a_refresh_prunes_what_left_and_embeds_what_arrived() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let built = two_documents()?;
    let declarations = two_declarations();
    whole_pass(&index, &built, &described(&built, &declarations), REVISION).await?;
    let kept = digest_of(&declarations[0]);
    let removed = digest_of(&declarations[1]);
    mark(root.path(), &kept).await?;

    let documents = vec![
        symbol("one", "src/one.rs", "load_config", "fn load config")?,
        symbol("three", "src/three.rs", "search_index", "fn search index")?,
    ];
    let arrived = Declaration::new("fn", "search_index").source("fn search index");
    let declarations = vec![declarations[0], arrived];
    incremental_pass(
        &index,
        &documents,
        &described(&documents, &declarations),
        REVISION,
    )
    .await?;

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
    let documents = two_documents()?;
    let declarations = two_declarations();
    let described = described(&documents, &declarations);
    whole_pass(&index, &documents, &described, REVISION).await?;
    let carried = digest_of(&declarations[0]);
    mark(root.path(), &carried).await?;

    whole_pass(&index, &documents, &described, REVISION).await?;
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
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Preparing {
            prepared: 1,
            total: 2
        },
        "stopping at the bound is reported, not swallowed"
    );
    assert_eq!(stored(root.path(), "model").await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_disabled_tier_answers_in_the_full_text_order_alone() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), lexical_only_limits()).await?;
    assert_eq!(index.pass_readiness(), VectorReadiness::Disabled);
    index
        .prepare(&model_source(root.path(), "model")?, acquisition_limits())
        .await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Disabled,
        "a disabled tier acquires nothing"
    );
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(index.pass_readiness(), VectorReadiness::Disabled);
    assert!(
        stored(root.path(), "model").await?.is_empty(),
        "a disabled tier embeds nothing"
    );

    let ranking = ranked(&index, "fn", 10).await?;
    let carried = input(&ranking, RankingInputKind::Lexical)?;
    assert_eq!(
        identities(carried),
        lexical_order(root.path(), "fn", 10).await?,
        "the full-text input is what the store alone ranked"
    );
    assert_eq!(
        fields_of(carried, "one")?,
        FieldSet::of(SearchableField::DeclarationSource)
    );
    assert!(
        !input(&ranking, RankingInputKind::Vector)?.answered(),
        "a disabled tier contributes an input that answered nothing"
    );
    Ok(())
}

/// The fixture model's vectors carry no trained meaning: its two layers hold
/// values this suite wrote, so nothing here measures relevance. What it proves
/// is that the vector ranking reaches the store's answer at all, for a query
/// the full-text tier cannot answer.
#[tokio::test]
async fn a_query_the_full_text_tier_cannot_answer_is_still_ranked_through_the_vector_ranking()
-> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;

    assert!(
        lexical_order(root.path(), "search", 10).await?.is_empty(),
        "the query shares no token with either document"
    );
    assert_eq!(
        vector_reached(&index, "search", 10).await?,
        ["one", "two"],
        "the vector ranking reaches documents the full-text tier cannot"
    );
    Ok(())
}

#[tokio::test]
async fn a_vector_lands_on_the_document_whose_declaration_produced_it() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let documents = vec![
        text_chunk("doc-one", "docs/one.md", "notes about loading")?,
        symbol("sym-one", "src/one.rs", "load_config", "fn load config")?,
        text_chunk("doc-two", "docs/two.md", "notes about reading")?,
        symbol("sym-two", "src/two.rs", "read_index", "fn read index")?,
    ];
    let declarations = two_declarations();
    let described = vec![
        DescribedUnit::new(&documents[1], declarations[0]),
        DescribedUnit::new(&documents[3], declarations[1]),
    ];
    whole_pass(&index, &documents, &described, REVISION).await?;

    assert_eq!(stored(root.path(), "model").await?.len(), 2);
    assert!(lexical_order(root.path(), "search", 10).await?.is_empty());
    let reached = vector_reached(&index, "search", 10).await?;
    assert_eq!(
        reached,
        ["sym-one", "sym-two"],
        "pairing by position would have put these vectors on doc-one and sym-one: {reached:?}"
    );
    Ok(())
}

#[tokio::test]
async fn more_documents_than_described_entries_leave_the_undescribed_ones_without_a_vector()
-> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let documents = vec![
        symbol("sym-one", "src/one.rs", "load_config", "fn load config")?,
        text_chunk("doc-one", "docs/one.md", "notes about loading")?,
        text_chunk("doc-two", "docs/two.md", "notes about reading")?,
    ];
    let declarations = two_declarations();
    let described = vec![DescribedUnit::new(&documents[0], declarations[0])];
    whole_pass(&index, &documents, &described, REVISION).await?;

    let vectors = stored(root.path(), "model").await?;
    assert_eq!(
        vectors.len(),
        1,
        "a document no declaration describes is not embedded"
    );
    assert_eq!(vectors[0].digest(), digest_of(&declarations[0]));
    assert_eq!(
        vector_reached(&index, "search", 10).await?,
        ["sym-one"],
        "the only vector resolves to the only described document"
    );
    assert_eq!(index.pass_readiness(), VectorReadiness::Ready);
    Ok(())
}

#[tokio::test]
async fn more_described_entries_than_documents_still_land_each_vector_on_its_own_document()
-> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let indexed = vec![symbol(
        "one",
        "src/one.rs",
        "load_config",
        "fn load config",
    )?];
    let apart = symbol("two", "src/two.rs", "read_index", "fn read index")?;
    let declarations = two_declarations();
    let described = vec![
        DescribedUnit::new(&indexed[0], declarations[0]),
        DescribedUnit::new(&apart, declarations[1]),
    ];
    whole_pass(&index, &indexed, &described, REVISION).await?;

    let vectors = stored(root.path(), "model").await?;
    let held: Vec<&str> = vectors.iter().map(StoredVector::digest).collect();
    assert_eq!(
        held.len(),
        2,
        "every described document is embedded: {held:?}"
    );
    assert_eq!(
        vector_reached(&index, "search", 10).await?,
        ["one", "two"],
        "a described document the lexical set never held still ranks under its own address"
    );
    Ok(())
}

#[tokio::test]
async fn a_tier_that_will_not_load_leaves_the_full_text_ranking_serving() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Preparing {
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
    assert_eq!(index.pass_readiness(), VectorReadiness::Unavailable);

    assert_eq!(
        lexical_identities(&index, "load config", 10).await?,
        ["one"],
        "the full-text tier keeps answering"
    );
    assert!(
        stored(root.path(), "model").await?.is_empty(),
        "an unavailable tier embeds nothing"
    );
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Unavailable,
        "one failure is final for the life of this index"
    );
    Ok(())
}

#[tokio::test]
async fn a_query_carrying_no_member_answers_nothing_and_a_limit_of_one_answers_once() -> TestResult
{
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;

    let blank = ranked(&index, "   ", 10).await?;
    assert!(
        blank.inputs().is_empty(),
        "a query of blanks carries no member, so no input ran: {blank:?}"
    );
    assert_eq!(blank.lexical_truncated_at(), None);
    let punctuation = ranked(&index, "-- ...", 10).await?;
    assert!(punctuation.inputs().is_empty(), "{punctuation:?}");

    assert_eq!(lexical_identities(&index, "load config", 1).await?.len(), 1);
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
    let documents = vec![
        symbol("crowd-one", "src/crowd.rs", "load", "fn load")?,
        symbol("crowd-two", "src/crowd.rs", "config", "fn config")?,
        symbol("crowd-three", "src/crowd.rs", "read", "fn read")?,
        symbol("lone", "src/lone.rs", "index", "fn index")?,
    ];
    let declarations = vec![
        Declaration::new("fn", "load").source("fn load"),
        Declaration::new("fn", "config").source("fn config"),
        Declaration::new("fn", "read").source("fn read"),
        Declaration::new("fn", "index").source("fn index"),
    ];
    whole_pass(
        &index,
        &documents,
        &described(&documents, &declarations),
        REVISION,
    )
    .await?;

    assert!(lexical_order(root.path(), "search", 10).await?.is_empty());
    let reached = vector_reached(&index, "search", 10).await?;
    assert_eq!(
        reached.len(),
        2,
        "one file contributes one candidate: {reached:?}"
    );
    assert!(
        reached.contains(&"lone".to_owned()),
        "the crowded file cannot push the other one out: {reached:?}"
    );
    Ok(())
}

#[tokio::test]
async fn readiness_walks_from_preparing_to_ready() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), limits()).await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Preparing {
            prepared: 0,
            total: 0
        }
    );
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(
        index.pass_readiness(),
        VectorReadiness::Preparing {
            prepared: 0,
            total: 2
        }
    );
    index
        .prepare(&model_source(root.path(), "model")?, acquisition_limits())
        .await?;
    incremental_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(index.pass_readiness(), VectorReadiness::Ready);
    Ok(())
}

#[tokio::test]
async fn vectors_with_no_document_to_rank_them_as_leave_the_full_text_ranking_alone() -> TestResult
{
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    drop(index);

    let reopened = prepared(root.path(), limits()).await?;
    assert_eq!(
        stored(root.path(), "model").await?.len(),
        2,
        "the vectors the first index wrote are still stored"
    );
    assert!(
        vector_reached(&reopened, "search", 10).await?.is_empty(),
        "no pass has said which document each digest belongs to"
    );
    assert_eq!(
        lexical_identities(&reopened, "load config", 10).await?,
        ["one"],
        "the full-text tier answers on its own"
    );
    Ok(())
}

#[tokio::test]
async fn a_model_change_drops_the_vectors_the_previous_model_wrote() -> TestResult {
    let root = workspace()?;
    write_model(&root.path().join("other"))?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
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
    whole_pass(
        &changed,
        fixture.documents(),
        &fixture.described(),
        REVISION,
    )
    .await?;
    assert_eq!(stored(root.path(), "other").await?.len(), 2);
    Ok(())
}

/// A query reads no vector row. What proves it is the store: the rows one pass
/// wrote are deleted underneath the index, and the same query answers with the
/// same documents afterwards, so what it ranked was the corpus that pass
/// published.
#[tokio::test]
async fn a_query_ranks_from_the_held_corpus_after_the_stored_rows_are_gone() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert!(
        lexical_order(root.path(), "search", 10).await?.is_empty(),
        "the query shares no token with either document, so only the vector ranking can answer it"
    );
    let answered = vector_reached(&index, "search", 10).await?;
    assert_eq!(answered, ["one", "two"]);

    drop_stored_vectors(root.path(), "model").await?;
    assert!(
        stored(root.path(), "model").await?.is_empty(),
        "no vector row is left for a query to read"
    );
    assert_eq!(
        vector_reached(&index, "search", 10).await?,
        answered,
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
    let built = vec![symbol(
        "one",
        "src/one.rs",
        "load_config",
        "fn load config",
    )?];
    let carried = Declaration::new("fn", "load_config").source("fn load config");
    whole_pass(&index, &built, &described(&built, &[carried]), REVISION).await?;

    let documents = vec![
        symbol("one", "src/one.rs", "load_config", "fn load config")?,
        symbol("three", "src/three.rs", "read_index", "fn read index")?,
    ];
    let arrived = Declaration::new("fn", "read_index").source("fn read index");
    incremental_pass(
        &index,
        &documents,
        &described(&documents, &[carried, arrived]),
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
        vector_reached(&index, "search", 10).await?,
        ["one", "three"],
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
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(vector_reached(&index, "search", 10).await?, ["one", "two"]);

    index
        .prepare(&model_source(root.path(), "other")?, acquisition_limits())
        .await?;
    assert!(
        vector_reached(&index, "search", 10).await?.is_empty(),
        "the corpus the previous model filled is held nowhere"
    );
    assert_eq!(
        lexical_identities(&index, "load config", 10).await?,
        ["one"],
        "the full-text tier keeps answering"
    );

    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(
        vector_reached(&index, "search", 10).await?,
        ["one", "two"],
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
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert_eq!(stored(root.path(), "model").await?.len(), 1);

    drop_stored_vectors(root.path(), "model").await?;
    assert!(lexical_order(root.path(), "search", 10).await?.is_empty());
    assert_eq!(
        vector_reached(&index, "search", 10).await?,
        ["one"],
        "the corpus carries the one vector the bound left room for, and no more"
    );
    Ok(())
}

#[tokio::test]
async fn a_store_refusal_carries_the_stores_own_violation() -> TestResult {
    let root = workspace()?;
    let narrow = SearchIndexLimits::builder(LexicalIndexLimits::new(1, 1 << 20, 32, 4, 1_000))
        .disable_vector()
        .build();
    let index = opened(root.path(), narrow).await?;
    let fixture = two()?;
    let error = whole_pass(&index, fixture.documents(), &fixture.described(), REVISION)
        .await
        .expect_err("two documents pass the one-document bound");
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
/// publishing too many documents that the server had failed, when the caller
/// could have published fewer. The registry identity and the limit evidence
/// travel with the failure so the answer stays actionable.
#[tokio::test]
async fn a_store_bound_keeps_its_registry_identity_and_its_limit_evidence() -> TestResult {
    let root = workspace()?;
    let narrow = SearchIndexLimits::builder(LexicalIndexLimits::new(1, 1 << 20, 32, 4, 1_000))
        .disable_vector()
        .build();
    let index = opened(root.path(), narrow).await?;
    let fixture = two()?;
    let error = whole_pass(&index, fixture.documents(), &fixture.described(), REVISION)
        .await
        .expect_err("two documents pass the one-document bound");

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
async fn a_rank_for_a_tree_the_store_moved_past_names_the_stored_revision() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;

    let parsed = ParsedQuery::parse("load config")?;
    let answered = index
        .rank("another-revision", &parsed, QueryPhase::Precise, 10)
        .await?;
    assert_eq!(answered, RevisionScoped::OtherRevision(REVISION.to_owned()));
    Ok(())
}

/// A store no pass has ever stamped answers for no tree at all, which is not the same as a
/// store holding another one: nothing has landed in it yet.
#[tokio::test]
async fn a_rank_before_any_population_reports_no_revision() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;

    let parsed = ParsedQuery::parse("load config")?;
    let answered = index
        .rank(REVISION, &parsed, QueryPhase::Precise, 10)
        .await?;
    assert_eq!(answered, RevisionScoped::NoRevision);
    Ok(())
}

/// Embedding runs after publication, so a newly published tree meets a corpus described
/// for the previous one. That corpus ranks nothing: the full-text tier answers alone until
/// the pass for this tree lands.
#[tokio::test]
async fn a_corpus_described_for_the_previous_tree_ranks_nothing() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;
    assert!(
        lexical_order(root.path(), "search", 10).await?.is_empty(),
        "the query shares no token with either document, so only the vector ranking can answer it"
    );
    assert_eq!(vector_reached(&index, "search", 10).await?.len(), 2);

    // The lexical half of the next publication lands first, as the publication path runs it.
    index
        .replace_lexical(fixture.documents(), "rev-two")
        .await?;
    let ranking = revision_ranking(&index, "rev-two", "search", QueryPhase::Precise, 10).await?;
    assert!(
        identities(input(&ranking, RankingInputKind::Vector)?).is_empty(),
        "the previous tree's vectors must not rank a tree they were not described for"
    );

    index
        .embed_described(&fixture.described(), Embedding::Missing, "rev-two")
        .await?;
    let ranking = revision_ranking(&index, "rev-two", "search", QueryPhase::Precise, 10).await?;
    assert_eq!(
        identities(input(&ranking, RankingInputKind::Vector)?).len(),
        2,
        "the pass for this tree publishes a corpus that ranks it"
    );
    Ok(())
}

/// One published set, both phases: the precise phase answers the document
/// carrying every term, and the broad phase widens the unquoted terms until
/// either document answers.
#[tokio::test]
async fn the_precise_phase_requires_every_term_and_the_broad_phase_widens_them() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), lexical_only_limits()).await?;
    let documents = [
        text_document("docs/guide.md", "alpha configuration guide")?,
        text_document("docs/other.md", "some other release notes")?,
    ];
    index.replace_lexical(&documents, REVISION).await?;

    let precise = phase_ranking(&index, "alpha other", QueryPhase::Precise, 10).await?;
    assert!(
        identities(input(&precise, RankingInputKind::Lexical)?).is_empty(),
        "the precise phase requires every term, and no document carries both"
    );

    let broad = phase_ranking(&index, "alpha other", QueryPhase::Broad, 10).await?;
    assert_eq!(
        reached(input(&broad, RankingInputKind::Lexical)?),
        ["docs/guide.md", "docs/other.md"],
        "the broad phase widens the unquoted terms and reaches either document"
    );

    let carried = phase_ranking(&index, "alpha configuration", QueryPhase::Precise, 10).await?;
    assert_eq!(
        identities(input(&carried, RankingInputKind::Lexical)?),
        ["docs/guide.md"],
        "the precise phase answers the document carrying every term"
    );
    Ok(())
}

#[tokio::test]
async fn a_quoted_phrase_stays_one_phrase_in_both_phases() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), lexical_only_limits()).await?;
    let documents = [
        text_document("docs/adjacent.md", "the alpha beacon reports nightly")?,
        text_document("docs/apart.md", "beacon first and alpha second")?,
    ];
    index.replace_lexical(&documents, REVISION).await?;

    for phase in [QueryPhase::Precise, QueryPhase::Broad] {
        let ranking = phase_ranking(&index, "\"alpha beacon\"", phase, 10).await?;
        let label = phase.label();
        assert_eq!(
            identities(input(&ranking, RankingInputKind::Lexical)?),
            ["docs/adjacent.md"],
            "a quoted phrase reaches adjacent words alone: phase={label}"
        );
    }
    Ok(())
}

/// An embedding reads the whole question, so widening the unquoted terms would
/// produce the vector order the precise phase already contributed.
#[tokio::test]
async fn the_broad_phase_runs_the_full_text_input_alone() -> TestResult {
    let root = workspace()?;
    let index = prepared(root.path(), limits()).await?;
    let fixture = two()?;
    whole_pass(&index, fixture.documents(), &fixture.described(), REVISION).await?;

    let broad = phase_ranking(&index, "load config", QueryPhase::Broad, 10).await?;
    assert_eq!(kinds(&broad), [RankingInputKind::Lexical]);

    let precise = ranked(&index, "load config", 10).await?;
    assert_eq!(
        kinds(&precise),
        [RankingInputKind::Lexical, RankingInputKind::Vector]
    );
    assert!(
        input(&precise, RankingInputKind::Vector)?.answered(),
        "a published corpus answers beside the full-text input: {precise:?}"
    );
    Ok(())
}

/// The full-text input carries the columns that placed each identity, so a
/// name hit, a documentation hit, and a file-content hit stay apart.
#[tokio::test]
async fn the_full_text_input_names_the_column_that_carried_the_term() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), lexical_only_limits()).await?;
    let named = symbol(
        "crate::beacon",
        "src/beacon.rs",
        "beacon",
        "pub fn declare() {}",
    )?;
    let documented = project_document(
        "crate::relay",
        "src/relay.rs",
        DocumentKind::Symbol,
        DocumentFields::empty()
            .with(SearchableField::Name, "relay")
            .with(
                SearchableField::Documentation,
                "forwards every beacon it receives",
            )
            .with(SearchableField::DeclarationSource, "pub fn relay() {}"),
    )?;
    let noted = text_document("docs/notes.md", "the beacon reports nightly")?;
    index
        .replace_lexical(&[named, documented, noted], REVISION)
        .await?;

    let ranking = ranked(&index, "beacon", 10).await?;
    let carried = input(&ranking, RankingInputKind::Lexical)?;
    assert_eq!(
        reached(carried),
        ["crate::beacon", "crate::relay", "docs/notes.md"],
        "all three documents carry the term somewhere"
    );
    assert_eq!(
        fields_of(carried, "crate::beacon")?,
        FieldSet::of(SearchableField::Name)
    );
    assert_eq!(
        fields_of(carried, "crate::relay")?,
        FieldSet::of(SearchableField::Documentation)
    );
    assert_eq!(
        fields_of(carried, "docs/notes.md")?,
        FieldSet::of(SearchableField::FileContent)
    );
    Ok(())
}

/// The bound the full-text ranking stopped at travels with the answer, so a
/// caller tells a full answer from a cut one without knowing the bound.
#[tokio::test]
async fn the_full_text_ranking_reports_the_bound_it_stopped_at() -> TestResult {
    let root = workspace()?;
    let index = opened(root.path(), lexical_only_limits()).await?;
    let fixture = two()?;
    index.replace_lexical(fixture.documents(), REVISION).await?;

    let cut = ranked(&index, "fn", 1).await?;
    assert_eq!(
        identities(input(&cut, RankingInputKind::Lexical)?).len(),
        1,
        "the bound keeps one match: {cut:?}"
    );
    assert_eq!(
        cut.lexical_truncated_at(),
        Some(1),
        "the store held a match past the bound"
    );

    let whole = ranked(&index, "fn", 10).await?;
    assert_eq!(
        identities(input(&whole, RankingInputKind::Lexical)?).len(),
        2
    );
    assert_eq!(
        whole.lexical_truncated_at(),
        None,
        "every match was ranked, so nothing was cut"
    );
    Ok(())
}
