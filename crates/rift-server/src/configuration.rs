//! Loads and validates the workspace's `rift.toml` and the environment
//! variables that override it.
//!
//! The document's shape and the variables naming its keys are accepted by
//! [`rift_core::acceptance`]; this module owns the filesystem half, reading
//! the file and treating a missing one as no document, and the workspace's
//! own value bounds.

use std::path::Path;

use rift_core::Error;
pub use rift_core::acceptance::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ConfigurationFault,
};
use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_core::constants::WORKSPACE_CONFIGURATION_FILE;
use rift_protocol::configuration::{ConfigurationViolation, WorkspaceConfiguration};

/// Reads `<root>/rift.toml` and the process's `RIFT_*` variables into the
/// validated configuration. A missing file is no document: the defaults, and
/// any variable overriding them.
///
/// # Errors
///
/// Returns [`ConfigurationError`] when the file cannot be read, is larger
/// than configuration can be, or is not the documented shape; when a
/// variable naming a key is malformed or names no key; or when a value breaks
/// a documented bound.
pub fn load_configuration(root: &Path) -> Result<WorkspaceConfiguration, ConfigurationError> {
    let document = read_document(root)?;
    accept_workspace(
        document.as_deref(),
        &ConfigurationEnvironment::from_process(),
    )
}

/// The file's text, or `None` when the workspace has no `rift.toml`.
fn read_document(root: &Path) -> Result<Option<String>, ConfigurationError> {
    let path = root.join(WORKSPACE_CONFIGURATION_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) if path.is_dir() => Err(Error::new(ConfigurationFault::IsDirectory {
            path: path.display().to_string(),
        })),
        Err(error) => Err(Error::new(ConfigurationFault::Unreadable {
            path: path.display().to_string(),
            io: error.to_string(),
        })),
    }
}

/// Accepts one workspace configuration: the document and variables, then
/// every value bound and the log capture filter. A refusal of a bound names
/// the variables that overrode a key, since the broken value may be theirs.
fn accept_workspace(
    document: Option<&str>,
    environment: &ConfigurationEnvironment,
) -> Result<WorkspaceConfiguration, ConfigurationError> {
    let (configuration, variables) =
        accept_configuration::<WorkspaceConfiguration>(document, environment)?.into_parts();
    let invalid = |violation| {
        Error::new(ConfigurationFault::Invalid {
            violation,
            variables: variables.clone(),
        })
    };
    configuration.validate().map_err(invalid)?;
    tracing_subscriber::EnvFilter::try_new(&configuration.logs.capture).map_err(|error| {
        invalid(ConfigurationViolation::LogCaptureInvalid {
            capture: configuration.logs.capture.clone(),
            detail: error.to_string(),
        })
    })?;
    Ok(configuration)
}

#[cfg(test)]
mod tests {
    /// Accepts one document with no variable overriding it.
    fn accept(raw: &str) -> Result<WorkspaceConfiguration, ConfigurationError> {
        accept_workspace(Some(raw), &ConfigurationEnvironment::default())
    }

    /// The repository's own `rift.toml`, exercised so the committed file accepts cleanly
    /// under the exact model this module validates against.
    #[test]
    fn test_repository_rift_toml_accepts_cleanly() {
        let raw = include_str!("../../../rift.toml");
        let configuration = accept(raw).expect("the repository's rift.toml must accept cleanly");
        assert!(configuration.source.include.is_empty());
        let excluded: Vec<&str> = configuration
            .source
            .exclude
            .iter()
            .map(|pattern| pattern.0.as_str())
            .collect();
        assert_eq!(
            excluded,
            [
                ".claude/**",
                ".agents/**",
                "docs/public/**",
                "plugins/claude/skills/**"
            ]
        );
        assert!(configuration.source.respect_gitignore);
    }

    #[test]
    fn test_documented_example_file_is_accepted() {
        let directory = tempfile::tempdir().expect("tempdir");
        let contents = r#"
[execution]
max_code = "16kb"
max_timeout = "30s"
max_output = "8kb"
max_concurrent = 2

[providers.history]
enabled = true
max_revisions = 500

[search.ranking]
identifier_weight = 0.3
lexical_weight = 0.6
vector_weight = 0.4

[search.vector.embedding]
kind = "hf"
model = "BAAI/bge-small-en-v1.5"
download_timeout = "5m"
"#;
        std::fs::write(directory.path().join("rift.toml"), contents).expect("configuration");
        let configuration =
            load_configuration(directory.path()).expect("the documented example must be accepted");
        assert_eq!(
            configuration.execution.max_code,
            ByteSize::from_bytes(16 << 10)
        );
        let ranking = &configuration.search.ranking;
        assert!((ranking.identifier_weight - 0.3).abs() < f64::EPSILON);
        assert!((ranking.lexical_weight - 0.6).abs() < f64::EPSILON);
        assert!((ranking.vector_weight - 0.4).abs() < f64::EPSILON);
        assert_eq!(
            configuration.search.vector.embedding,
            EmbeddingConfiguration::Hf {
                model: "BAAI/bge-small-en-v1.5".to_owned(),
                download_timeout: Duration::from_millis(300_000),
                download_attempts: 3,
                batch_inputs: 32,
                max_tokens: 256,
            }
        );
    }

    use super::*;
    use rift_core::{ErrorCode, ErrorName};
    use rift_protocol::configuration::{
        ByteSize, Duration, EmbeddingConfiguration, WorkspaceConfiguration,
    };

    #[test]
    fn test_missing_file_is_the_default_configuration() {
        let directory = tempfile::tempdir().expect("tempdir");
        let configuration =
            load_configuration(directory.path()).expect("a missing file must accept defaults");
        assert_eq!(configuration, WorkspaceConfiguration::default());
    }

    #[test]
    fn test_vector_candidate_bounds_parse_from_toml() {
        let configuration = accept("[search.vector]\ncandidates = 100\ncandidates_per_file = 8\n")
            .expect("both candidate bounds must be accepted");
        assert_eq!(configuration.search.vector.candidates, 100);
        assert_eq!(configuration.search.vector.candidates_per_file, 8);
    }

    #[test]
    fn test_unknown_key_is_refused_as_malformed() {
        let error = accept("[execution]\nmax_codes = \"16kb\"\n")
            .expect_err("an unknown key must refuse the file");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Malformed { .. }
        ));
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        let message = error.to_string();
        assert!(
            message.contains("key execution") && message.contains("location line 2 column 1"),
            "the refusal names the table and the line the unknown key stands on: {message}"
        );
        assert!(
            message.contains("accepted max_code, max_concurrent, max_output, max_timeout"),
            "the refusal names the keys that table accepts: {message}"
        );
        assert!(
            message.contains(r#"example {"max_code":"16kb""#),
            "the refusal shows a value that table takes: {message}"
        );
        assert!(
            !message.contains("unknown field") && !message.contains("expected one of"),
            "a refusal never speaks serde's grammar: {message}"
        );
    }

    #[test]
    fn test_toml_syntax_error_is_refused_as_malformed() {
        let error = accept("[execution\n").expect_err("a syntax error must refuse the file");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Malformed { .. }
        ));
        let message = error.to_string();
        assert!(
            message.contains("location line 1 column "),
            "a syntax error names where the parser stopped: {message}"
        );
    }

    /// A value of the wrong shape is refused by its own key, with a value
    /// that key takes. The refusal never names a Rust type: `u64` and
    /// `Duration` are names only this repository holds.
    #[test]
    fn test_a_value_of_the_wrong_shape_names_its_key_and_a_value_it_takes() {
        let error = accept("[server]\nnum_workers = \"four\"\n")
            .expect_err("a value of the wrong shape must refuse the file");
        let message = error.to_string();
        assert!(
            message.contains("key server.num_workers"),
            "the refusal names the key: {message}"
        );
        assert!(
            message.contains("location line 2 column "),
            "the refusal names where the parser stopped: {message}"
        );
        assert!(
            message.contains("example "),
            "the refusal shows a value the key takes: {message}"
        );
        assert!(
            !message.contains("invalid type") && !message.contains("u64"),
            "a refusal never speaks serde's grammar or names a Rust type: {message}"
        );
    }

    #[test]
    fn test_oversized_file_is_refused_before_parsing() {
        let oversized = "# padding\n".repeat(1 << 15);
        assert!(oversized.len() as u64 > CONFIGURATION_FILE_BYTES_MAX);
        let error = accept(&oversized).expect_err("an oversized file must be refused");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Oversized { .. }
        ));
        let message = error.to_string();
        assert!(
            message.contains("bytes") && message.contains("bytes_max 262144"),
            "the refusal must name the size and the accepted maximum: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_file_is_a_storage_failure() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join(WORKSPACE_CONFIGURATION_FILE);
        fs::write(&path, "[server]\n").expect("test configuration must be writable");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .expect("fixture permissions set");
        let error = load_configuration(directory.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("fixture permissions restore");
        let error = error.expect_err("a file this process cannot read must fail to read");
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::StorageFailure));
        let message = error.to_string();
        assert!(
            message.contains("path ") && message.contains("io "),
            "the refusal must carry the path and the I/O account: {message}"
        );
    }

    #[test]
    fn test_directory_in_place_of_the_file_is_configuration_invalid() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join(WORKSPACE_CONFIGURATION_FILE))
            .expect("a directory can shadow the configuration file");
        let error = load_configuration(directory.path())
            .expect_err("a directory in the file's place must be refused, not retried");
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid),
            "a directory can never become readable by retrying"
        );
        let message = error.to_string();
        let expected_path = directory
            .path()
            .join(WORKSPACE_CONFIGURATION_FILE)
            .display()
            .to_string();
        assert!(
            message.contains(&expected_path),
            "the refusal must name the offending path: {message}"
        );
        assert!(
            message.contains("directory"),
            "the refusal must say the path is a directory: {message}"
        );
    }

    #[test]
    fn test_invalid_log_capture_filter_is_refused() {
        let error = accept("[logs]\ncapture = \"[\"\n")
            .expect_err("an invalid tracing filter must refuse the file");
        assert!(
            matches!(
                error.fault(),
                ConfigurationFault::Invalid {
                    violation: ConfigurationViolation::LogCaptureInvalid { capture, detail },
                    ..
                } if capture == "[" && !detail.is_empty()
            ),
            "unexpected configuration failure: {error:?}"
        );
    }

    #[test]
    fn test_a_bound_broken_by_a_variable_names_the_variable() {
        let environment =
            ConfigurationEnvironment::from_variables([("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "0")]);
        let error = accept_workspace(Some("[providers.history]\nenabled = true\n"), &environment)
            .expect_err("zero nodes breaks the documented bound");
        assert!(
            matches!(
                error.fault(),
                ConfigurationFault::Invalid { variables, .. }
                    if variables == &["RIFT_PROVIDERS_SYNTAX_MAX_NODES"]
            ),
            "unexpected configuration failure: {error:?}"
        );
    }

    #[test]
    fn test_a_variable_prevails_over_the_file() {
        let environment = ConfigurationEnvironment::from_variables([(
            "RIFT_PROVIDERS_SYNTAX_MAX_NODES",
            "5000000",
        )]);
        let configuration =
            accept_workspace(Some("[providers.syntax]\nmax_nodes = 1000\n"), &environment)
                .expect("the variable's value is in bounds");
        assert_eq!(configuration.providers.syntax.max_nodes, 5_000_000);
    }
}
