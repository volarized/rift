//! Loads and validates the workspace's `rift.toml` and the environment
//! variables that override it.
//!
//! The document's shape and the variables naming its keys are accepted by
//! [`rift_core::acceptance`]; this module owns the filesystem half, reading
//! the file and treating a missing one as no document, and the workspace's
//! own value bounds.

use std::path::{Path, PathBuf};

pub use rift_core::acceptance::CONFIGURATION_FILE_BYTES_MAX;
use rift_core::acceptance::{ConfigurationEnvironment, NamedMembers, accept_configuration_naming};
use rift_core::constants::WORKSPACE_CONFIGURATION_FILE;
use rift_error::{ErrorContext, RiftError, errors};
use rift_protocol::configuration::{ConfigurationViolation, WorkspaceConfiguration};

use crate::history_fill::release_matcher;

pub(crate) const CONFIGURATION_PARENTS_MAX: usize =
    rift_protocol::configuration::PROFILE_ROOT_DEPTH_MAX + 1;

/// Reads the repository root's `rift.toml` and the process's `RIFT_*` variables
/// into the selected project's configuration. A profile uses its root-file
/// settings. A missing file uses defaults and any variable overriding them.
///
/// # Errors
///
/// Returns [`RiftError`] when the file cannot be read, is larger
/// than configuration can be, or is not the documented shape; when a
/// variable naming a key is malformed or names no key; or when a value breaks
/// a documented bound.
pub fn load_configuration(root: &Path) -> Result<WorkspaceConfiguration, RiftError> {
    let normalized = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root = normalized.as_path();
    let file = configuration_file(root);
    let configuration_root = file.parent().unwrap_or(root);
    let document = read_document(configuration_root)?;
    let configuration = accept_workspace(
        document.as_deref(),
        &ConfigurationEnvironment::from_process(),
    )?;
    if configuration_root == root {
        validate_profile_roots(root, &configuration)?;
        let configuration = configuration.with_profile_exclusions();
        configuration
            .validate()
            .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
        return Ok(configuration);
    }
    let selected =
        std::fs::canonicalize(root).map_err(|error| profile_invalid("", &error.to_string()))?;
    let profiles = profile_roots(configuration_root, &configuration)?;
    profiles
        .into_iter()
        .find(|(_, path)| *path == selected)
        .and_then(|(name, _)| configuration.for_profile(&name))
        .map_or_else(
            || accept_workspace(None, &ConfigurationEnvironment::from_process()),
            Ok,
        )
}

/// Returns the repository root configuration file for one selected project.
/// At most 64 parent folders are inspected. Without Git metadata, the nearest
/// configuration file defines the root; an absent file uses the project defaults.
#[must_use]
pub fn configuration_file(root: &Path) -> PathBuf {
    let normalized = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root = normalized.as_path();
    let mut configuration = None;
    for candidate in root.ancestors().take(CONFIGURATION_PARENTS_MAX) {
        let path = candidate.join(WORKSPACE_CONFIGURATION_FILE);
        if candidate.join(".git").exists() {
            return path;
        }
        if configuration.is_none() && path.exists() {
            configuration = Some(path);
        }
    }
    configuration.unwrap_or_else(|| root.join(WORKSPACE_CONFIGURATION_FILE))
}

/// Resolves profiles to distinct folders below the root. Canonical paths cannot overlap.
///
/// # Errors
/// Returns a configuration refusal for an unavailable, escaping, or overlapping folder.
pub fn profile_roots(
    root: &Path,
    configuration: &WorkspaceConfiguration,
) -> Result<std::collections::BTreeMap<String, PathBuf>, RiftError> {
    if configuration.profiles.is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }
    let primary =
        std::fs::canonicalize(root).map_err(|error| profile_invalid("", &error.to_string()))?;
    let mut roots = std::collections::BTreeMap::<String, PathBuf>::new();
    for (name, profile) in &configuration.profiles {
        let path = std::fs::canonicalize(primary.join(&profile.root))
            .map_err(|error| profile_invalid(name, &error.to_string()))?;
        if path == primary || !path.starts_with(&primary) || !path.is_dir() {
            return Err(profile_invalid(
                name,
                "profile root must name a folder below this project",
            ));
        }
        if path != primary.join(&profile.root) {
            return Err(profile_invalid(
                name,
                "profile root must use its canonical folder path",
            ));
        }
        if path
            .ancestors()
            .take(CONFIGURATION_PARENTS_MAX)
            .take_while(|folder| *folder != primary)
            .any(|folder| folder.join(".git").exists())
        {
            return Err(profile_invalid(
                name,
                "profile root crosses a repository boundary; register the project in mcp.extra",
            ));
        }
        if roots
            .values()
            .any(|other| other.starts_with(&path) || path.starts_with(other))
        {
            return Err(profile_invalid(
                name,
                "profile roots overlap; select separate project folders",
            ));
        }
        roots.insert(name.clone(), path);
    }
    Ok(roots)
}

fn validate_profile_roots(
    root: &Path,
    configuration: &WorkspaceConfiguration,
) -> Result<(), RiftError> {
    profile_roots(root, configuration).map(|_| ())
}

fn profile_invalid(name: &str, detail: &str) -> RiftError {
    errors::core::configuration_invalid()
        .field("profiles")
        .name(name)
        .detail(detail)
        .error()
}

/// The file's text, or `None` when the workspace has no `rift.toml`.
fn read_document(root: &Path) -> Result<Option<String>, RiftError> {
    let path = root.join(WORKSPACE_CONFIGURATION_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) if path.is_dir() => errors::core::configuration_is_directory()
            .file(WORKSPACE_CONFIGURATION_FILE)
            .path(path.display().to_string())
            .detail("directory")
            .fail(),
        Err(error) => errors::core::configuration_unreadable()
            .file(WORKSPACE_CONFIGURATION_FILE)
            .path(path.display().to_string())
            .io(&error)
            .source(error)
            .fail(),
    }
}

/// Accepts one workspace configuration: the document and variables, then
/// every value bound, the history release patterns, and the log capture
/// filter. A refusal of a bound names the variables that overrode a key, since
/// the broken value may be theirs.
///
/// The protocol model checks a release pattern's form alone; whether it
/// compiles is decided here, through the compile a `selective` fill runs, so
/// a pattern no fill could use refuses the file rather than leaving the
/// history store unfilled.
fn accept_workspace(
    document: Option<&str>,
    environment: &ConfigurationEnvironment,
) -> Result<WorkspaceConfiguration, RiftError> {
    let shipped: Vec<String> = rift_syntax::definitions()
        .iter()
        .map(|definition| definition.shipped().language().identity_segment())
        .collect();
    let languages: Vec<&str> = shipped.iter().map(String::as_str).collect();
    let members = [NamedMembers {
        table: "languages",
        names: &languages,
    }];
    let (configuration, variables) =
        accept_configuration_naming::<WorkspaceConfiguration>(document, environment, &members)?
            .into_parts();
    if let Err(violation) = configuration.validate() {
        let mut error = rift_core::configuration_violation_error(&violation)
            .with(ErrorContext::new("file", WORKSPACE_CONFIGURATION_FILE));
        if !variables.is_empty() {
            error = error.with(ErrorContext::new("variables", variables.join(", ")));
        }
        return error.fail();
    }
    let releases = &configuration.providers.history.releases;
    if let Some((pattern, error)) = releases
        .iter()
        .find_map(|pattern| release_matcher(pattern).err().map(|error| (pattern, error)))
    {
        let violation = ConfigurationViolation::HistoryReleasePatternInvalid {
            pattern: pattern.clone(),
            detail: error.to_string(),
        };
        let mut error = rift_core::configuration_violation_error(&violation)
            .with(ErrorContext::new("file", WORKSPACE_CONFIGURATION_FILE));
        if !variables.is_empty() {
            error = error.with(ErrorContext::new("variables", variables.join(", ")));
        }
        return error.fail();
    }
    if let Err(error) = rift_tracing::validate_log_filter(&configuration.logs.capture) {
        let violation = ConfigurationViolation::LogCaptureInvalid {
            capture: configuration.logs.capture.clone(),
            detail: error.to_string(),
        };
        let mut error = rift_core::configuration_violation_error(&violation)
            .with(ErrorContext::new("file", WORKSPACE_CONFIGURATION_FILE));
        if !variables.is_empty() {
            error = error.with(ErrorContext::new("variables", variables.join(", ")));
        }
        return error.fail();
    }
    Ok(configuration)
}

#[cfg(test)]
mod tests {
    /// Accepts one document with no variable overriding it.
    fn accept(raw: &str) -> Result<WorkspaceConfiguration, RiftError> {
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
                ".codex/**",
                "docs/public/**",
                "plugins/claude/skills/**",
                "dev/**",
                "docs/**"
            ]
        );
        assert!(configuration.source.respect_gitignore);
    }

    /// Every `rift.toml` the repository keeps, by its path from the repository root.
    const REPOSITORY_CONFIGURATIONS: [(&str, &str); 1] =
        [("rift.toml", include_str!("../../../rift.toml"))];

    /// A server started in any directory of this repository that holds a `rift.toml` reads
    /// it under the model this module validates against, so each one accepts cleanly.
    #[test]
    fn test_every_repository_rift_toml_accepts_cleanly() {
        for (path, raw) in REPOSITORY_CONFIGURATIONS {
            if let Err(error) = accept(raw) {
                panic!("{path} must accept cleanly: {error}");
            }
        }
    }

    /// [`REPOSITORY_CONFIGURATIONS`] names every `rift.toml` below the repository root that
    /// `.gitignore` keeps, so a file added anywhere in the tree joins the acceptance check.
    #[test]
    fn test_the_repository_configurations_name_every_rift_toml_in_the_tree() {
        let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo sets CARGO_MANIFEST_DIR for every test it runs");
        let root = std::path::Path::new(&manifest).join("../..");
        let mut walk = ignore::WalkBuilder::new(&root);
        walk.standard_filters(false)
            .git_ignore(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git");
        let mut found: Vec<String> = walk
            .build()
            .map(|entry| entry.expect("the repository tree must be readable"))
            .filter(|entry| entry.file_name() == WORKSPACE_CONFIGURATION_FILE)
            .map(|entry| {
                let relative = entry
                    .path()
                    .strip_prefix(&root)
                    .expect("the walk stays below its root");
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect();
        found.sort();
        let listed: Vec<&str> = REPOSITORY_CONFIGURATIONS
            .iter()
            .map(|(path, _)| *path)
            .collect();
        assert_eq!(
            found, listed,
            "every rift.toml in the tree must be listed in REPOSITORY_CONFIGURATIONS"
        );
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
        assert_eq!(error.slug(), errors::core::configuration_malformed::SLUG);
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
        assert_eq!(error.slug(), errors::core::configuration_malformed::SLUG);
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
        assert_eq!(error.slug(), errors::core::configuration_oversized::SLUG);
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
        assert_eq!(error.slug(), errors::core::configuration_unreadable::SLUG);
        assert!(std::error::Error::source(&error).is_some());
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
        assert_eq!(error.slug(), errors::core::configuration_is_directory::SLUG);
        let message = error.to_string();
        let expected_path = directory
            .path()
            .canonicalize()
            .expect("fixture root must resolve")
            .join(WORKSPACE_CONFIGURATION_FILE)
            .display()
            .to_string();
        let actual_path = error
            .context()
            .find_map(|(key, value)| (key == "path").then_some(value))
            .expect("directory refusal carries its path");
        assert_eq!(
            actual_path, expected_path,
            "the refusal must name the resolved configuration path"
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
        assert_eq!(
            error.slug(),
            errors::core::configuration_log_capture_invalid::SLUG
        );
        let context: Vec<_> = error.context().collect();
        assert!(context.contains(&("capture", "[".to_owned())));
        assert!(
            context
                .iter()
                .any(|(key, value)| *key == "detail" && !value.is_empty())
        );
    }

    #[test]
    fn test_a_release_pattern_that_does_not_compile_is_refused() {
        let document = concat!(
            "[providers.history]\n",
            "strategy = \"selective\"\n",
            "releases = [\"v*\", \"v[1\"]\n",
        );
        let error = accept(document).expect_err("an unclosed class compiles into no matcher");
        assert_eq!(
            error.slug(),
            errors::core::configuration_history_release_pattern_invalid::SLUG
        );
        let context: Vec<_> = error.context().collect();
        assert!(context.contains(&("pattern", "v[1".to_owned())));
        assert!(context.iter().any(|(key, value)| {
            *key == "detail" && value.contains("unclosed character class")
        }));
        let rendered = error.to_string();
        assert!(
            rendered.contains("providers.history.releases"),
            "{rendered}"
        );
        assert!(
            rendered.contains("unclosed character class"),
            "the refusal carries the glob parser's account: {rendered}"
        );
        let alternation = concat!(
            "[providers.history]\n",
            "strategy = \"selective\"\n",
            "releases = [\"v{1,2}.*\"]\n",
        );
        assert!(accept(alternation).is_ok(), "an alternation compiles");
    }

    #[test]
    fn test_a_bound_broken_by_a_variable_names_the_variable() {
        let environment =
            ConfigurationEnvironment::from_variables([("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "0")]);
        let error = accept_workspace(Some("[providers.history]\nenabled = true\n"), &environment)
            .expect_err("zero nodes breaks the documented bound");
        assert_eq!(
            error.slug(),
            errors::core::configuration_limit_out_of_range::SLUG
        );
        assert!(error.context().any(|(key, value)| {
            key == "variables" && value == "RIFT_PROVIDERS_SYNTAX_MAX_NODES"
        }));
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

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn profiles_refuse_nested_repository_boundaries() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("docs/.git"))?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[profiles.docs]\nroot = 'docs'\n",
        )?;
        let error =
            load_configuration(root.path()).expect_err("nested repository must use mcp.extra");
        assert!(error.to_string().contains("repository boundary"), "{error}");
        Ok(())
    }

    #[test]
    fn deepest_accepted_profile_loads_its_root_file() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let relative =
            std::iter::repeat_n("d", rift_protocol::configuration::PROFILE_ROOT_DEPTH_MAX)
                .collect::<Vec<_>>()
                .join("/");
        std::fs::create_dir_all(root.path().join(&relative))?;
        std::fs::write(
            root.path().join("rift.toml"),
            format!("[telemetry]\ndisabled = true\n[profiles.docs]\nroot = '{relative}'\n"),
        )?;
        assert!(
            load_configuration(&root.path().join(&relative))?
                .telemetry
                .disabled
        );
        Ok(())
    }

    #[test]
    fn derived_profile_exclusions_refuse_source_pattern_overflow()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("docs"))?;
        let patterns = (0..rift_protocol::source::SOURCE_PATTERNS_MAX)
            .map(|index| format!("'cache-{index}/**'"))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            root.path().join("rift.toml"),
            format!("[source]\nexclude = [{patterns}]\n[profiles.docs]\nroot = 'docs'\n"),
        )?;
        let error = load_configuration(root.path())
            .expect_err("derived exclusions must stay within source bounds");
        let context: std::collections::BTreeMap<_, _> = error.context().collect();
        assert_eq!(
            context.get("field").map(String::as_str),
            Some("source.exclude")
        );
        assert_eq!(context.get("value").map(String::as_str), Some("513"));
        Ok(())
    }

    #[test]
    fn profiles_load_root_configuration_without_child_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join(".git"))?;
        std::fs::create_dir(root.path().join("docs"))?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[server]\nport = 39123\n[profiles.docs]\nroot = 'docs'\n[profiles.docs.languages.typescript]\nlsp.command = ['typescript-language-server', '--stdio']\n",
        )?;
        let primary = load_configuration(root.path())?;
        assert!(
            primary
                .source
                .exclude
                .iter()
                .any(|pattern| pattern.0 == "docs/**")
        );
        let profile = load_configuration(&root.path().join("docs"))?;
        assert!(profile.languages["typescript"].lsp.is_some());
        assert!(profile.source.exclude.is_empty());
        assert_eq!(profile.server.port, None);
        assert_eq!(profile.server.port_range, None);
        assert!(profile.profiles.is_empty());
        assert!(!root.path().join("docs/rift.toml").exists());
        assert_eq!(
            configuration_file(&root.path().join("docs")),
            root.path().canonicalize()?.join("rift.toml")
        );
        Ok(())
    }

    #[test]
    fn profile_configuration_changes_follow_root_file() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("docs"))?;
        let path = root.path().join("rift.toml");
        for disabled in [true, false] {
            std::fs::write(
                &path,
                format!("[telemetry]\ndisabled = {disabled}\n[profiles.docs]\nroot = 'docs'\n"),
            )?;
            assert_eq!(
                load_configuration(&root.path().join("docs"))?
                    .telemetry
                    .disabled,
                disabled
            );
        }
        Ok(())
    }

    #[test]
    fn unrelated_child_folders_keep_defaults() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("docs/src"))?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[telemetry]\ndisabled = true\n[profiles.docs]\nroot = 'docs'\n",
        )?;
        assert_eq!(
            load_configuration(&root.path().join("docs/src"))?,
            WorkspaceConfiguration::default()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn profiles_refuse_symlink_escapes_and_alias_overlap() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("docs"))?;
        symlink(outside.path(), root.path().join("escape"))?;
        symlink(root.path().join("docs"), root.path().join("alias"))?;
        for text in [
            "[profiles.docs]\nroot = 'escape'\n",
            "[profiles.docs]\nroot = 'docs'\n[profiles.alias]\nroot = 'alias'\n",
        ] {
            std::fs::write(root.path().join("rift.toml"), text)?;
            assert!(load_configuration(root.path()).is_err());
        }
        Ok(())
    }

    #[test]
    fn profile_search_stops_at_repository_boundary() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("nested/.git"))?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[telemetry]\ndisabled = true\n",
        )?;
        assert_eq!(
            load_configuration(&root.path().join("nested"))?,
            WorkspaceConfiguration::default()
        );
        Ok(())
    }
}
