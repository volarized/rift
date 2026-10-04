//! The public acquisition surface, driven with no network and no process
//! environment, so no suite can address the machine's own Hugging Face cache.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_error::RiftError;
use rift_error::errors;
use rift_search::{AcquisitionLimits, FetchedFile, ModelSource, acquire};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// The three files an encoder loads, in the spelling the crate already uses.
const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// The repository the operator's default names.
const REPOSITORY: &str = "BAAI/bge-small-en-v1.5";

/// Limits that spend no wall clock.
fn limits(attempts: u32) -> AcquisitionLimits {
    AcquisitionLimits::new(
        Duration::from_secs(30),
        attempts,
        Duration::from_millis(250),
        Duration::from_secs(8),
    )
}

/// The repository and revision one accepted identifier resolved to.
fn resolved(model: &str) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    match ModelSource::repository(model)? {
        ModelSource::Repository {
            repository,
            revision,
        } => Ok((repository, revision)),
        other @ ModelSource::Directory(_) => {
            Err(format!("`{model}` must resolve to a repository: {other:?}").into())
        }
    }
}

/// The directory one accepted relative path resolved to below `root`.
fn resolved_directory(
    model: &str,
    root: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    match ModelSource::directory(model, root)? {
        ModelSource::Directory(directory) => Ok(directory),
        other @ ModelSource::Repository { .. } => {
            Err(format!("`{model}` must resolve to a directory: {other:?}").into())
        }
    }
}

/// The refusal one identifier earned, with its exact evidence and message.
fn refused(source: Result<ModelSource, RiftError>, model: &str, expected: &str) {
    let error = source.expect_err(&format!("`{model}` must be refused"));
    assert_eq!(
        error.slug(),
        errors::search::model_source_invalid::SLUG,
        "`{model}` must be refused as an invalid model source"
    );
    let message = format!("model source {model} has invalid form; expected {expected}");
    assert_eq!(
        error.message(),
        message,
        "the refusal states its expected form"
    );
    let evidence = error.context().collect::<Vec<_>>();
    assert_eq!(evidence.len(), 2, "the refusal keeps both evidence fields");
    assert!(evidence.contains(&("model", model.to_owned())));
    assert!(evidence.contains(&("expected", expected.to_owned())));
    let rendered = error.to_string();
    assert_eq!(
        rendered,
        format!("{message}; use a model source in the expected form and retry")
    );
}

#[test]
fn a_repository_identifier_without_a_revision_reads_at_main() -> TestResult {
    assert_eq!(
        resolved(REPOSITORY)?,
        (REPOSITORY.to_owned(), "main".to_owned())
    );
    Ok(())
}

#[test]
fn a_repository_identifier_names_the_revision_after_the_separator() -> TestResult {
    assert_eq!(
        resolved("BAAI/bge-small-en-v1.5@refs-pr-5")?,
        (REPOSITORY.to_owned(), "refs-pr-5".to_owned())
    );
    assert_eq!(
        resolved("owner/name@0e0ac2cbe0ee1c1d")?,
        ("owner/name".to_owned(), "0e0ac2cbe0ee1c1d".to_owned())
    );
    Ok(())
}

#[test]
fn a_repository_identifier_missing_an_owner_or_a_name_is_refused() {
    for model in ["/name", "owner/", "/"] {
        refused(
            ModelSource::repository(model),
            model,
            "a non-empty owner and name, as in `BAAI/bge-small-en-v1.5`",
        );
    }
    refused(
        ModelSource::repository("@main"),
        "@main",
        "the form `owner/name`, as in `BAAI/bge-small-en-v1.5`",
    );
}

#[test]
fn a_repository_identifier_with_the_wrong_segment_count_is_refused() {
    for model in ["name", "owner/name/extra", ""] {
        refused(
            ModelSource::repository(model),
            model,
            "the form `owner/name`, as in `BAAI/bge-small-en-v1.5`",
        );
    }
}

#[test]
fn a_repository_identifier_with_more_than_one_separator_is_refused() {
    refused(
        ModelSource::repository("owner/name@one@two"),
        "owner/name@one@two",
        "one `@` at most, as in `owner/name@revision`",
    );
}

#[test]
fn a_repository_identifier_with_an_empty_revision_is_refused() {
    refused(
        ModelSource::repository("owner/name@"),
        "owner/name@",
        "a non-empty revision after `@`",
    );
}

#[test]
fn a_repository_identifier_whose_revision_carries_a_separator_is_refused() {
    let model = "owner/name@branch/one";
    refused(
        ModelSource::repository(model),
        model,
        "a revision carrying no `/`",
    );
}

#[test]
fn a_repository_identifier_carrying_a_dot_segment_is_refused() {
    for model in ["../name", "owner/..", "./name", "owner/.", "owner/name@.."] {
        refused(
            ModelSource::repository(model),
            model,
            "no `.` or `..` segment",
        );
    }
}

#[test]
fn a_relative_directory_is_resolved_against_the_workspace_root() -> TestResult {
    let root = Path::new("/workspace");
    assert_eq!(
        resolved_directory("models/bge-small", root)?,
        root.join("models/bge-small")
    );
    assert_eq!(resolved_directory("model", root)?, root.join("model"));
    Ok(())
}

#[test]
fn a_directory_that_is_not_relative_or_not_canonical_is_refused() {
    let root = Path::new("/workspace");
    for (model, expected) in [
        ("", "a non-empty path"),
        ("/absolute", "a path relative to the workspace root"),
        ("C:/absolute", "a path relative to the workspace root"),
        ("models\\bge", "`/` as the separator"),
        ("../outside", "no `.` or `..` segment"),
        ("models/../bge", "no `.` or `..` segment"),
        ("models/./bge", "no `.` or `..` segment"),
        ("models//bge", "no empty segment"),
        ("models/\u{7}bge", "no control character"),
    ] {
        refused(ModelSource::directory(model, root), model, expected);
    }
}

#[test]
fn the_limits_report_what_they_were_built_with() {
    let bounds = limits(4);
    assert_eq!(bounds.timeout(), Duration::from_secs(30));
    assert_eq!(bounds.attempts(), 4);
    assert_eq!(bounds.retry_delay(), Duration::from_millis(250));
    assert_eq!(bounds.retry_delay_limit(), Duration::from_secs(8));
}

#[test]
fn the_first_delay_is_the_base_and_every_later_one_doubles() {
    let bounds = limits(6);
    assert_eq!(bounds.delay_after(1), Some(Duration::from_millis(250)));
    assert_eq!(bounds.delay_after(2), Some(Duration::from_millis(500)));
    assert_eq!(bounds.delay_after(3), Some(Duration::from_secs(1)));
    assert_eq!(bounds.delay_after(4), Some(Duration::from_secs(2)));
}

#[test]
fn a_delay_grows_no_further_than_its_ceiling() {
    let bounds = AcquisitionLimits::new(
        Duration::from_secs(30),
        u32::MAX,
        Duration::from_millis(250),
        Duration::from_millis(600),
    );
    assert_eq!(bounds.delay_after(2), Some(Duration::from_millis(500)));
    assert_eq!(bounds.delay_after(3), Some(Duration::from_millis(600)));
    assert_eq!(
        bounds.delay_after(4_000),
        Some(Duration::from_millis(600)),
        "growth that overflows saturates and the ceiling clamps it"
    );
}

#[test]
fn a_spent_attempt_bound_has_no_delay_left() {
    assert_eq!(
        limits(1).delay_after(1),
        None,
        "one attempt allows no retry"
    );
    assert_eq!(limits(3).delay_after(3), None);
    assert_eq!(limits(3).delay_after(9), None);
    assert_eq!(
        AcquisitionLimits::new(Duration::ZERO, 0, Duration::ZERO, Duration::ZERO).delay_after(1),
        None
    );
}

#[tokio::test]
async fn a_directory_source_loads_the_three_files_it_holds() -> TestResult {
    let root = tempfile::tempdir()?;
    let directory = root.path().join("models/bge-small");
    std::fs::create_dir_all(&directory)?;
    for file in MODEL_FILES {
        std::fs::write(directory.join(file), file.as_bytes())?;
    }
    let source = ModelSource::directory("models/bge-small", root.path())?;
    let files = acquire(&source, limits(1)).await?;
    let rendered = format!("{files:?}");
    for file in MODEL_FILES {
        assert!(rendered.contains(file), "{file} must be named: {rendered}");
    }
    Ok(())
}

#[tokio::test]
async fn a_directory_source_short_of_one_file_names_the_file_that_is_missing() -> TestResult {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("model"))?;
    std::fs::write(root.path().join("model/config.json"), b"{}")?;
    let source = ModelSource::directory("model", root.path())?;
    let error = acquire(&source, limits(1))
        .await
        .expect_err("a directory short of a file cannot load");
    assert_eq!(error.slug(), errors::search::model_file_missing::SLUG);
    assert!(error.to_string().contains("tokenizer.json"), "{error}");
    Ok(())
}

#[test]
fn a_fetched_file_reports_the_commit_and_etag_it_was_built_with() {
    let fetched = FetchedFile::new(Some("0e0ac2c".to_owned()), Some("d41d8cd9".to_owned()));
    assert_eq!(fetched.commit(), Some("0e0ac2c"));
    assert_eq!(fetched.etag(), Some("d41d8cd9"));
    let declared_nothing = FetchedFile::new(None, None);
    assert_eq!(declared_nothing.commit(), None);
    assert_eq!(declared_nothing.etag(), None);
    assert_eq!(declared_nothing, FetchedFile::default());
}

#[test]
fn the_debug_render_names_a_source_a_bound_and_a_fetched_file() -> TestResult {
    let repository = format!("{:?}", ModelSource::repository("owner/name@abc")?);
    assert!(repository.contains("Repository"), "{repository}");
    assert!(repository.contains("owner/name"), "{repository}");
    assert!(repository.contains("abc"), "{repository}");

    let directory = format!("{:?}", ModelSource::directory("model", Path::new("/root"))?);
    assert!(directory.contains("Directory"), "{directory}");
    assert!(directory.contains("model"), "{directory}");

    let bounds = format!("{:?}", limits(3));
    assert!(bounds.contains("AcquisitionLimits"), "{bounds}");
    assert!(bounds.contains("attempts"), "{bounds}");

    let fetched = format!("{:?}", FetchedFile::new(Some("abc".to_owned()), None));
    assert!(fetched.contains("FetchedFile"), "{fetched}");
    assert!(fetched.contains("abc"), "{fetched}");
    Ok(())
}

#[test]
fn every_acquisition_violation_renders_its_own_message() {
    let cases = [
        errors::search::model_source_invalid()
            .model("test model")
            .expected("repository")
            .error(),
        errors::search::model_cache_unavailable()
            .variables("HF_HUB_CACHE, HF_HOME, XDG_CACHE_HOME, HOME, USERPROFILE")
            .error(),
        errors::search::model_download_failed()
            .subject("test file")
            .error(),
        errors::search::model_download_too_large()
            .url("https://example.test/model")
            .bytes_max(64_u64)
            .error(),
    ];
    for error in cases {
        let rendered = error.to_string();
        assert!(
            error.slug().as_str().starts_with("rift.search."),
            "registered search slug: {error:?}"
        );
        assert!(!rendered.is_empty(), "registered error renders its message");
    }
}
