//! Live integration: one checkpoint reached two ways answers one encoding.
//!
//! `RIFT_SEARCH_LIVE=1 cargo nextest run -p rift-search --test live_model_sources`
//! runs the suite; without the variable every test skips visibly. It acquires
//! the default model from the hub, copies the three acquired files into a
//! workspace directory, and loads both. The two are the same bytes reached two
//! ways, so they must encode identically.
//!
//! What they must not share is their embedding space. A repository carries the
//! commit its origin resolved and a directory carries the digest over its own
//! files, so the two identities differ and a corpus embedded through one is
//! never scored against a query embedded through the other.

use std::error::Error;
use std::path::Path;
use std::time::Duration;

use rift_search::{
    AcquisitionLimits, Encoder, EncoderLimits, ModelFiles, ModelSource, acquire,
    local_embedding_space,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The environment variable that turns the live model tests on.
const SEARCH_LIVE_VARIABLE: &str = "RIFT_SEARCH_LIVE";

/// The repository this suite acquires; the workspace's own default.
const REPOSITORY: &str = "minishlab/potion-retrieval-32M";

/// The three files an encoder loads, in the order it loads them.
const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// The text both encoders embed.
const TEXT: &str = "fn resolve_workspace_root(path: &Path) -> Option<PathBuf>";

/// Texts one forward pass takes.
const BATCH_DECLARATIONS: usize = 4;
/// Tokens one text reaches the model with.
const TOKENS_MAX: usize = 256;
/// Texts one call takes.
const TEXTS_MAX: usize = 16;
/// Wall clock one file fetch receives.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
/// Attempts one file fetch makes.
const FETCH_ATTEMPTS: u32 = 3;
/// The first delay between two fetch attempts.
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// The longest delay between two fetch attempts.
const RETRY_DELAY_LIMIT: Duration = Duration::from_secs(8);
/// The largest difference two coordinates of one encoding may carry.
const COORDINATE_TOLERANCE: f32 = 0.0;

/// Whether the live model tests run; an unset gate prints the skip line.
fn search_live() -> bool {
    if std::env::var_os(SEARCH_LIVE_VARIABLE).is_some() {
        return true;
    }
    eprintln!("skipped: {SEARCH_LIVE_VARIABLE} unset");
    false
}

fn limits() -> AcquisitionLimits {
    AcquisitionLimits::new(
        FETCH_TIMEOUT,
        FETCH_ATTEMPTS,
        RETRY_DELAY,
        RETRY_DELAY_LIMIT,
    )
}

fn encoder_limits() -> EncoderLimits {
    EncoderLimits::new(BATCH_DECLARATIONS, TOKENS_MAX, TEXTS_MAX)
}

/// Copies the three acquired files beside each other in `directory`.
fn copy_beside(acquired: &Path, directory: &Path) -> TestResult {
    std::fs::create_dir_all(directory)?;
    for name in MODEL_FILES {
        std::fs::copy(acquired.join(name), directory.join(name))?;
    }
    Ok(())
}

#[tokio::test]
async fn a_repository_and_a_directory_holding_its_files_encode_one_text_alike() -> TestResult {
    if !search_live() {
        return Ok(());
    }
    let source = ModelSource::repository(REPOSITORY)?;
    let acquired = acquire(&source, limits()).await?;
    let snapshot = acquired
        .directory()
        .ok_or("the acquired files sit in a snapshot directory")?
        .to_path_buf();

    let root = tempfile::tempdir()?;
    let held = root.path().join("models/potion-retrieval-32M");
    copy_beside(&snapshot, &held)?;
    let directory = ModelSource::Directory(held.clone());
    let directory_files = ModelFiles::in_directory(&held)?;

    let from_repository = Encoder::load(&acquired, encoder_limits())?;
    let from_directory = Encoder::load(&directory_files, encoder_limits())?;
    assert_eq!(
        from_repository.dimension(),
        from_directory.dimension(),
        "one checkpoint has one width"
    );
    assert_eq!(
        from_repository.query_transformation(),
        from_directory.query_transformation(),
        "one checkpoint transforms a query one way"
    );

    let repository_vector = from_repository.embed_documents(&[TEXT.to_owned()])?;
    let directory_vector = from_directory.embed_documents(&[TEXT.to_owned()])?;
    assert_eq!(repository_vector.len(), 1);
    assert_eq!(repository_vector.len(), directory_vector.len());
    for (left, right) in repository_vector[0].iter().zip(&directory_vector[0]) {
        assert!(
            (left - right).abs() <= COORDINATE_TOLERANCE,
            "one checkpoint answers one coordinate: {left} against {right}"
        );
    }

    let repository_space = local_embedding_space(
        &source,
        &acquired,
        from_repository.dimension(),
        from_repository.query_transformation(),
    );
    let directory_space = local_embedding_space(
        &directory,
        &directory_files,
        from_directory.dimension(),
        from_directory.query_transformation(),
    );
    assert_eq!(
        repository_space.dimensions(),
        directory_space.dimensions(),
        "one checkpoint has one width in either space"
    );
    assert_ne!(
        repository_space.identity(),
        directory_space.identity(),
        "a resolved commit and a file digest address two spaces"
    );
    assert_ne!(
        acquired.revision(),
        REPOSITORY,
        "the acquired files carry the resolved commit, not the repository name"
    );
    Ok(())
}
