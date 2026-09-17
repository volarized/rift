//! Loads and validates the workspace's `rift.toml`.
//!
//! The file's shape and bounds are the protocol's
//! [`WorkspaceConfiguration`]; this module owns the filesystem half: reading
//! the file, refusing one the model cannot accept, and treating a missing
//! file as the default configuration.

use std::path::Path;

use rift_core::constants::WORKSPACE_CONFIGURATION_FILE;
use rift_core::line::lines_inclusive;
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault};
use rift_protocol::configuration::{ConfigurationViolation, WorkspaceConfiguration};
use rift_protocol::schema::{
    DocumentStep, ExpectedShape, configuration_schema, document_steps, expected_shape, named_member,
};

/// Bytes a `rift.toml` may hold, at most. The file states bounded tables
/// and language entries; one this large is not configuration.
pub const CONFIGURATION_FILE_BYTES_MAX: u64 = 256 << 10;

/// One configuration failure: why the workspace's `rift.toml` cannot be
/// accepted.
#[derive(Debug)]
pub enum ConfigurationFault {
    /// The file exists but its bytes could not be read.
    Unreadable {
        /// The file's path.
        path: String,
        /// The rendered I/O failure.
        io: String,
    },
    /// A directory stands where the configuration file belongs. Reading it
    /// can never succeed by retrying: the operator must remove or replace
    /// it with the file.
    IsDirectory {
        /// The directory's path.
        path: String,
    },
    /// The file is larger than configuration can be.
    Oversized {
        /// The file's size in bytes.
        bytes: u64,
        /// The accepted maximum in bytes.
        bytes_max: u64,
    },
    /// The file is not the documented TOML shape: a syntax error, an
    /// unknown key, a missing required key, or a malformed value.
    Malformed {
        /// Where the parser stopped, as `line <n> column <n>`. Absent when
        /// the parser named no position in the file.
        location: Option<String>,
        /// The documented key the parser stopped inside, members joined by
        /// `.`. Absent when it stopped before reaching one.
        key: Option<String>,
        /// What the documented shape accepts there, and a value it takes.
        /// Boxed so one refusal's evidence does not widen every configuration
        /// `Result` the crate returns.
        shape: Box<ExpectedShape>,
    },
    /// The file parsed and one of its values breaks a documented bound.
    Invalid(ConfigurationViolation),
}

impl Fault for ConfigurationFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Unreadable { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
            Self::IsDirectory { .. }
            | Self::Oversized { .. }
            | Self::Malformed { .. }
            | Self::Invalid(_) => ErrorName::Wire(ErrorCode::ConfigurationInvalid),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("file", WORKSPACE_CONFIGURATION_FILE)];
        match self {
            Self::Unreadable { path, io } => {
                context.push(ErrorContext::new("path", path.clone()));
                context.push(ErrorContext::new("io", io.clone()));
            }
            Self::IsDirectory { path } => {
                context.push(ErrorContext::new("path", path.clone()));
                context.push(ErrorContext::new("detail", "the path is a directory"));
            }
            Self::Oversized { bytes, bytes_max } => {
                context.push(ErrorContext::new("bytes", bytes.to_string()));
                context.push(ErrorContext::new("bytes_max", bytes_max.to_string()));
            }
            Self::Malformed {
                location,
                key,
                shape,
            } => {
                if let Some(key) = key {
                    context.push(ErrorContext::new("key", key.clone()));
                }
                if let Some(location) = location {
                    context.push(ErrorContext::new("location", location.clone()));
                }
                if !shape.accepted().is_empty() {
                    context.push(ErrorContext::new("accepted", shape.accepted().join(", ")));
                }
                if let Some(example) = shape.example() {
                    context.push(ErrorContext::new("example", example.to_string()));
                }
            }
            Self::Invalid(violation) => context.extend(violation.context()),
        }
        context
    }
}

/// Opaque configuration failure.
pub type ConfigurationError = Error<ConfigurationFault>;

/// Reads `<root>/rift.toml` into the validated configuration. A missing
/// file is the default configuration; any other failure names what to fix.
///
/// # Errors
///
/// Returns [`ConfigurationError`] when the file cannot be read, is larger
/// than configuration can be, is not the documented shape, or breaks a
/// documented bound.
pub fn load_configuration(root: &Path) -> Result<WorkspaceConfiguration, ConfigurationError> {
    let path = root.join(WORKSPACE_CONFIGURATION_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceConfiguration::default());
        }
        Err(_) if path.is_dir() => {
            return Err(Error::new(ConfigurationFault::IsDirectory {
                path: path.display().to_string(),
            }));
        }
        Err(error) => {
            return Err(Error::new(ConfigurationFault::Unreadable {
                path: path.display().to_string(),
                io: error.to_string(),
            }));
        }
    };
    accept_configuration(&raw)
}

/// Reads the documented shape out of one configuration text.
///
/// A refusal names the key the parser stopped inside, where in the file it
/// stopped, what the documented shape accepts there, and a value it takes.
/// The parser's own account is not carried: it speaks of Rust types and serde
/// grammar, which name nothing the operator can write in `rift.toml`.
fn accept_shape(raw: &str) -> Result<WorkspaceConfiguration, ConfigurationError> {
    let deserializer = match toml::Deserializer::parse(raw) {
        Ok(deserializer) => deserializer,
        Err(error) => return Err(malformed(raw, error.span(), &[])),
    };
    match serde_path_to_error::deserialize(deserializer) {
        Ok(configuration) => Ok(configuration),
        Err(refused) => {
            let steps = document_steps(refused.path());
            Err(malformed(raw, refused.inner().span(), &steps))
        }
    }
}

/// One refusal of the documented shape, described against the schema the
/// workspace publishes.
fn malformed(
    raw: &str,
    span: Option<std::ops::Range<usize>>,
    steps: &[DocumentStep<'_>],
) -> ConfigurationError {
    let shape = expected_shape(&configuration_schema(), steps);
    Error::new(ConfigurationFault::Malformed {
        location: span.map(|span| position_of(raw, span.start)),
        key: named_member(&steps[..shape.followed()]),
        shape: Box::new(shape),
    })
}

/// Where one byte offset stands in the file, counting lines and columns from
/// one, the way an editor does.
fn position_of(raw: &str, offset: usize) -> String {
    let mut line = 1;
    let mut consumed = 0;
    for text in lines_inclusive(raw) {
        if consumed + text.len() > offset {
            break;
        }
        consumed += text.len();
        line += 1;
    }
    let column = raw[consumed..offset.min(raw.len())].chars().count() + 1;
    format!("line {line} column {column}")
}

/// Accepts one configuration text: size bound, shape, then value bounds.
/// Split from the read so every refusal path is testable without a
/// filesystem.
fn accept_configuration(raw: &str) -> Result<WorkspaceConfiguration, ConfigurationError> {
    let bytes = raw.len() as u64;
    if bytes > CONFIGURATION_FILE_BYTES_MAX {
        return Err(Error::new(ConfigurationFault::Oversized {
            bytes,
            bytes_max: CONFIGURATION_FILE_BYTES_MAX,
        }));
    }
    let configuration = accept_shape(raw)?;
    configuration
        .validate()
        .map_err(|violation| Error::new(ConfigurationFault::Invalid(violation)))?;
    tracing_subscriber::EnvFilter::try_new(&configuration.logs.capture).map_err(|error| {
        Error::new(ConfigurationFault::Invalid(
            ConfigurationViolation::LogCaptureInvalid {
                capture: configuration.logs.capture.clone(),
                detail: error.to_string(),
            },
        ))
    })?;
    Ok(configuration)
}

#[cfg(test)]
mod tests {
    /// The repository's own `rift.toml`, exercised so the committed file accepts cleanly
    /// under the exact model this module validates against.
    #[test]
    fn test_repository_rift_toml_accepts_cleanly() {
        let raw = include_str!("../../../rift.toml");
        let configuration =
            accept_configuration(raw).expect("the repository's rift.toml must accept cleanly");
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

[search.lexical]
weight = 0.6

[search.semantic]
weight = 0.4
source = "hf"
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
        assert!((configuration.search.lexical.weight - 0.6).abs() < f64::EPSILON);
        assert!((configuration.search.semantic.weight - 0.4).abs() < f64::EPSILON);
        assert_eq!(configuration.search.semantic.source, SemanticSource::Hf);
        assert_eq!(
            configuration.search.semantic.model,
            "BAAI/bge-small-en-v1.5"
        );
    }

    use super::*;
    use rift_protocol::configuration::{ByteSize, SemanticSource, WorkspaceConfiguration};

    #[test]
    fn test_missing_file_is_the_default_configuration() {
        let directory = tempfile::tempdir().expect("tempdir");
        let configuration =
            load_configuration(directory.path()).expect("a missing file must accept defaults");
        assert_eq!(configuration, WorkspaceConfiguration::default());
    }

    #[test]
    fn test_semantic_candidate_bounds_parse_from_toml() {
        let configuration =
            accept_configuration("[search.semantic]\ncandidates = 100\ncandidates_per_file = 8\n")
                .expect("both candidate bounds must be accepted");
        assert_eq!(configuration.search.semantic.candidates, 100);
        assert_eq!(configuration.search.semantic.candidates_per_file, 8);
    }

    #[test]
    fn test_unknown_key_is_refused_as_malformed() {
        let error = accept_configuration("[execution]\nmax_codes = \"16kb\"\n")
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
        let error =
            accept_configuration("[execution\n").expect_err("a syntax error must refuse the file");
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
        let error = accept_configuration("[server]\nnum_workers = \"four\"\n")
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

    /// A position is counted the way an editor counts, from one, and a byte
    /// offset inside a line lands on that line.
    #[test]
    fn test_a_position_counts_lines_and_columns_from_one() {
        let raw = "alpha\nbeta\ngamma\n";
        assert_eq!(position_of(raw, 0), "line 1 column 1");
        assert_eq!(position_of(raw, 6), "line 2 column 1");
        assert_eq!(position_of(raw, 9), "line 2 column 4");
        assert_eq!(position_of(raw, raw.len()), "line 4 column 1");
    }

    #[test]
    fn test_oversized_file_is_refused_before_parsing() {
        let oversized = "# padding\n".repeat(1 << 15);
        assert!(oversized.len() as u64 > CONFIGURATION_FILE_BYTES_MAX);
        let error =
            accept_configuration(&oversized).expect_err("an oversized file must be refused");
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
        let error = accept_configuration("[logs]\ncapture = \"[\"\n")
            .expect_err("an invalid tracing filter must refuse the file");
        assert!(
            matches!(
                error.fault(),
                ConfigurationFault::Invalid(ConfigurationViolation::LogCaptureInvalid {
                    capture,
                    detail,
                }) if capture == "[" && !detail.is_empty()
            ),
            "unexpected configuration failure: {error:?}"
        );
    }
}
