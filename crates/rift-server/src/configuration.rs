//! Loads and validates the workspace's `rift.toml`.
//!
//! The file's shape and bounds are the protocol's
//! [`WorkspaceConfiguration`]; this module owns the filesystem half: reading
//! the file, refusing one the model cannot admit, and treating a missing
//! file as the default configuration.

use std::path::Path;

use rift_core::constants::WORKSPACE_CONFIGURATION_FILE;
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault};
use rift_protocol::configuration::{ConfigurationViolation, WorkspaceConfiguration};

/// Bytes a `rift.toml` may hold, at most. The file states bounded tables
/// and hook lists; one this large is not configuration.
const CONFIGURATION_FILE_BYTES_MAX: u64 = 256 << 10;

/// One configuration failure: why the workspace's `rift.toml` cannot be
/// admitted.
#[derive(Debug)]
pub enum ConfigurationFault {
    /// The file exists but its bytes could not be read.
    Unreadable {
        /// The file's path.
        path: String,
        /// The rendered I/O failure.
        io: String,
    },
    /// The file is larger than configuration can be.
    Oversized {
        /// The file's size in bytes.
        bytes: u64,
        /// The admitted maximum in bytes.
        bytes_max: u64,
    },
    /// The file is not the documented TOML shape: a syntax error, an
    /// unknown key, a missing required key, or a malformed value.
    Malformed {
        /// The parser's account, with its line and column.
        detail: String,
    },
    /// The file parsed and one of its values breaks a documented bound.
    Invalid(ConfigurationViolation),
}

impl Fault for ConfigurationFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Unreadable { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
            Self::Oversized { .. } | Self::Malformed { .. } | Self::Invalid(_) => {
                ErrorName::Wire(ErrorCode::ConfigurationInvalid)
            }
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("file", WORKSPACE_CONFIGURATION_FILE)];
        match self {
            Self::Unreadable { path, io } => {
                context.push(ErrorContext::new("path", path.clone()));
                context.push(ErrorContext::new("io", io.clone()));
            }
            Self::Oversized { bytes, bytes_max } => {
                context.push(ErrorContext::new("bytes", bytes.to_string()));
                context.push(ErrorContext::new("bytes_max", bytes_max.to_string()));
            }
            Self::Malformed { detail } => {
                context.push(ErrorContext::new("detail", detail.clone()));
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
        Err(error) => {
            return Err(Error::new(ConfigurationFault::Unreadable {
                path: path.display().to_string(),
                io: error.to_string(),
            }));
        }
    };
    admit_configuration(&raw)
}

/// Admits one configuration text: size bound, shape, then value bounds.
/// Split from the read so every refusal path is testable without a
/// filesystem.
fn admit_configuration(raw: &str) -> Result<WorkspaceConfiguration, ConfigurationError> {
    let bytes = raw.len() as u64;
    if bytes > CONFIGURATION_FILE_BYTES_MAX {
        return Err(Error::new(ConfigurationFault::Oversized {
            bytes,
            bytes_max: CONFIGURATION_FILE_BYTES_MAX,
        }));
    }
    let configuration: WorkspaceConfiguration = toml::from_str(raw).map_err(|error| {
        Error::new(ConfigurationFault::Malformed {
            detail: error.to_string(),
        })
    })?;
    configuration
        .validate()
        .map_err(|violation| Error::new(ConfigurationFault::Invalid(violation)))?;
    Ok(configuration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::configuration::{ByteSize, ChangedPaths, Determinism, HookKind};

    /// The `[[hooks]]` example the configuration docs show, keys complete.
    const DOCUMENTED_HOOK: &str = r#"
[[hooks]]
type = "command"
id = "tests"
kind = "test"
program = "cargo"
arguments = ["test"]
changed_paths = "none"
working_directory = ""
environment = {}
timeout_ms = 120000
output_limit_bytes = 4096
guarantees = []
determinism = "deterministic"
"#;

    fn write_configuration(directory: &tempfile::TempDir, contents: &str) {
        std::fs::write(
            directory.path().join(WORKSPACE_CONFIGURATION_FILE),
            contents,
        )
        .expect("test configuration must be writable");
    }

    #[test]
    fn test_missing_file_is_the_default_configuration() {
        let directory = tempfile::tempdir().expect("tempdir");
        let configuration =
            load_configuration(directory.path()).expect("a missing file must admit defaults");
        assert_eq!(configuration, WorkspaceConfiguration::default());
    }

    #[test]
    fn test_documented_example_file_is_admitted() {
        let directory = tempfile::tempdir().expect("tempdir");
        let contents = format!(
            r#"
[execution]
allow = ["python"]
max_code = "16kb"
max_timeout = "30s"
max_output = "8kb"
max_concurrent = 2

[providers.history]
enabled = true
max_revisions = 500

[search]
embedding = "potion-retrieval-32M"
{DOCUMENTED_HOOK}
"#
        );
        write_configuration(&directory, &contents);
        let configuration =
            load_configuration(directory.path()).expect("the documented example must be admitted");
        assert_eq!(configuration.execution.allow, ["python"]);
        assert_eq!(
            configuration.execution.max_code,
            ByteSize::from_bytes(16 << 10)
        );
        assert_eq!(
            configuration.search.embedding.as_deref(),
            Some("potion-retrieval-32M")
        );
        let hook = &configuration.hooks[0];
        assert_eq!(hook.id, "tests");
        assert_eq!(hook.kind, HookKind::Test);
        assert_eq!(hook.program, "cargo");
        assert_eq!(hook.arguments, ["test"]);
        assert_eq!(hook.changed_paths, ChangedPaths::None);
        assert_eq!(hook.determinism, Determinism::Deterministic);
    }

    /// The repository's own `rift.toml`, exercised so the committed file admits cleanly
    /// under the exact model this module validates against.
    #[test]
    fn test_repository_rift_toml_admits_cleanly() {
        let raw = include_str!("../../../rift.toml");
        let configuration =
            admit_configuration(raw).expect("the repository's rift.toml must admit cleanly");
        assert!(configuration.source.include.is_empty());
        assert!(configuration.source.exclude.is_empty());
        assert!(configuration.source.respect_gitignore);
        assert_eq!(configuration.hooks.len(), 2);
        assert_eq!(configuration.hooks[0].id, "format");
        assert_eq!(configuration.hooks[0].program, "just");
        assert_eq!(configuration.hooks[0].arguments, ["format"]);
        assert_eq!(configuration.hooks[1].id, "check");
        assert_eq!(configuration.hooks[1].program, "just");
        assert_eq!(configuration.hooks[1].arguments, ["check"]);
    }

    #[test]
    fn test_unknown_key_is_refused_as_malformed() {
        let error = admit_configuration("[execution]\nmax_codes = \"16kb\"\n")
            .expect_err("an unknown key must refuse the file");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Malformed { .. }
        ));
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        assert!(
            error.to_string().contains("max_codes"),
            "the refusal must name the unknown key: {error}"
        );
    }

    #[test]
    fn test_toml_syntax_error_is_refused_as_malformed() {
        let error =
            admit_configuration("[execution\n").expect_err("a syntax error must refuse the file");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Malformed { .. }
        ));
    }

    #[test]
    fn test_hook_missing_required_key_is_refused() {
        let trimmed = DOCUMENTED_HOOK.replace("environment = {}\n", "");
        let error = admit_configuration(&trimmed)
            .expect_err("a hook without environment must refuse the file");
        assert!(
            error.to_string().contains("environment"),
            "the refusal must name the missing key: {error}"
        );
    }

    #[test]
    fn test_out_of_bounds_value_is_refused_with_field_evidence() {
        let broken = DOCUMENTED_HOOK.replace("timeout_ms = 120000", "timeout_ms = 0");
        let error =
            admit_configuration(&broken).expect_err("a zero hook timeout must refuse the file");
        assert!(matches!(error.fault(), ConfigurationFault::Invalid(_)));
        let message = error.to_string();
        assert!(
            message.contains("hooks.timeout_ms") && message.contains("1..=3600000"),
            "the refusal must name the field and its range: {message}"
        );
    }

    #[test]
    fn test_absolute_hook_executable_is_refused() {
        let broken = DOCUMENTED_HOOK.replace("program = \"cargo\"", "program = \"/bin/cargo\"");
        let error = admit_configuration(&broken)
            .expect_err("an absolute executable path must refuse the file");
        assert!(
            error.to_string().contains("/bin/cargo"),
            "the refusal must name the refused program: {error}"
        );
    }

    #[test]
    fn test_oversized_file_is_refused_before_parsing() {
        let oversized = "# padding\n".repeat(1 << 15);
        assert!(oversized.len() as u64 > CONFIGURATION_FILE_BYTES_MAX);
        let error = admit_configuration(&oversized).expect_err("an oversized file must be refused");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Oversized { .. }
        ));
        let message = error.to_string();
        assert!(
            message.contains("bytes") && message.contains("bytes_max 262144"),
            "the refusal must name the size and the admitted maximum: {message}"
        );
    }

    #[test]
    fn test_unreadable_file_is_a_storage_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join(WORKSPACE_CONFIGURATION_FILE))
            .expect("a directory can shadow the configuration file");
        let error = load_configuration(directory.path())
            .expect_err("a directory in the file's place must fail to read");
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::StorageFailure));
        let message = error.to_string();
        assert!(
            message.contains("path ") && message.contains("io "),
            "the refusal must carry the path and the I/O account: {message}"
        );
    }
}
