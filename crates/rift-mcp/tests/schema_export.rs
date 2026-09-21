//! Integration coverage for the `rift-schema-export` CLI logic: argument
//! parsing, writing, `--check`, and the errors each path can produce.

use std::error::Error;
use std::fs;
use std::process::Command;

use rift_mcp::schema::{self, ExportError};
use rift_mcp::skill::{self, SkillForm};
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const REGENERATE_COMMAND: &str = "just generate";

/// A write request keeping both export roots inside `directory`, so no test
/// touches the checkout's own committed artifacts through the defaults.
fn write_request(directory: &tempfile::TempDir) -> TestResult<schema::ExportRequest> {
    Ok(schema::parse_arguments([
        directory.path().display().to_string(),
        directory.path().join("plugin").display().to_string(),
    ])?)
}

/// The `--check` counterpart of [`write_request`].
fn check_request(directory: &tempfile::TempDir) -> TestResult<schema::ExportRequest> {
    Ok(schema::parse_arguments([
        "--check".to_owned(),
        directory.path().display().to_string(),
        directory.path().join("plugin").display().to_string(),
    ])?)
}

/// Every `rift://` identity the served document advertises spells its path with the one shared
/// character class. Four models carried that class by hand and one of them dropped `@`, so the
/// server returned node identities its own schema refused; a fifth identity cannot repeat it.
#[test]
fn every_served_identity_pattern_spells_its_path_with_the_shared_class() -> TestResult {
    fn patterns(node: &Value, found: &mut Vec<String>) {
        match node {
            Value::Object(members) => {
                for (key, value) in members {
                    match value {
                        Value::String(pattern) if key == "pattern" => found.push(pattern.clone()),
                        _ => patterns(value, found),
                    }
                }
            }
            Value::Array(entries) => entries.iter().for_each(|entry| patterns(entry, found)),
            _ => {}
        }
    }

    let mut found = Vec::new();
    patterns(
        &serde_json::from_str(&schema::schema_document())?,
        &mut found,
    );
    patterns(
        &serde_json::from_str(&schema::configuration_schema_document())?,
        &mut found,
    );
    let identities: Vec<&String> = found
        .iter()
        .filter(|pattern| pattern.starts_with("^rift://"))
        .collect();
    assert!(
        identities.len() >= 4,
        "the served document advertises one pattern per identity: {identities:?}"
    );
    for pattern in identities {
        assert!(
            pattern.contains(rift_protocol::read::IDENTITY_PATH_CHARACTER),
            "an identity pattern spells its path with the shared class: {pattern}"
        );
    }
    Ok(())
}

#[test]
fn run_without_arguments_writes_default_output_directory() -> TestResult {
    let directory = tempfile::tempdir()?;
    let output = Command::new(
        std::env::var_os("CARGO_BIN_EXE_rift-schema-export")
            .ok_or("test runner must provide CARGO_BIN_EXE_rift-schema-export")?,
    )
    .current_dir(directory.path())
    .output()?;
    assert!(output.status.success());

    let written = fs::read_to_string(directory.path().join("docs/public/mcp.json"))?;
    assert_eq!(written, schema::schema_document());
    let configuration = fs::read_to_string(directory.path().join("docs/public/rift.schema.json"))?;
    assert_eq!(configuration, schema::configuration_schema_document());
    let manifest = fs::read_to_string(
        directory
            .path()
            .join("plugins/claude/.claude-plugin/plugin.json"),
    )?;
    assert_eq!(manifest, skill::plugin_manifest());

    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.starts_with("wrote "));
    assert!(stdout.contains("docs/public/mcp.json"));
    assert!(stdout.contains("docs/public/rift.schema.json"));
    assert!(stdout.contains("SKILL.md"));
    Ok(())
}

#[test]
fn check_fails_when_configuration_schema_is_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;
    fs::write(directory.path().join("public/rift.schema.json"), "{}")?;

    let error =
        schema::run(&check_request(&directory)?).expect_err("stale configuration schema must fail");
    let ExportError::CheckMismatch { path } = error else {
        panic!("expected CheckMismatch, got {error:?}");
    };
    assert!(path.ends_with("rift.schema.json"));
    Ok(())
}

#[test]
fn configuration_schema_document_is_deterministic() -> TestResult {
    let first = schema::configuration_schema_document();
    assert_eq!(first, schema::configuration_schema_document());
    let document: Value = serde_json::from_str(&first)?;
    assert_eq!(document["title"], "WorkspaceConfiguration");
    Ok(())
}

#[test]
fn package_index_schema_document_is_deterministic() -> TestResult {
    let first = schema::package_index_schema_document();
    assert_eq!(first, schema::package_index_schema_document());
    let document: Value = serde_json::from_str(&first)?;
    assert_eq!(document["title"], "PackagePublication");
    Ok(())
}

#[test]
fn check_fails_when_the_package_index_schema_is_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;
    fs::write(
        directory.path().join("public/package-index.schema.json"),
        "{}",
    )?;

    let error =
        schema::run(&check_request(&directory)?).expect_err("a stale package index schema fails");
    let ExportError::CheckMismatch { path } = error else {
        panic!("expected CheckMismatch, got {error:?}");
    };
    assert!(path.ends_with("package-index.schema.json"));
    Ok(())
}

/// A tree holding every input the analyzer manifest names, taken from the committed
/// manifest itself: it lists each pinned source path, and the lockfile is copied from the
/// repository so the grammar pins are the shipped ones.
fn analyzer_root(repository: &std::path::Path) -> TestResult<tempfile::TempDir> {
    let committed: Value = serde_json::from_str(&fs::read_to_string(
        repository.join(rift_index::analyzer_manifest_path()),
    )?)?;
    let root = tempfile::tempdir()?;
    fs::copy(
        repository.join("Cargo.lock"),
        root.path().join("Cargo.lock"),
    )?;
    for source in committed["sources"]
        .as_array()
        .ok_or("the manifest lists sources")?
    {
        let relative = source["path"].as_str().ok_or("a source states its path")?;
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().ok_or("a source has a directory")?)?;
        fs::copy(repository.join(relative), path)?;
    }
    Ok(root)
}

/// The repository root this test binary was built from.
fn repository_root() -> TestResult<std::path::PathBuf> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    Ok(std::path::Path::new(&manifest)
        .ancestors()
        .nth(2)
        .ok_or("the crate sits two directories below the repository root")?
        .to_path_buf())
}

/// Writing the analyzer manifest below a root, then checking it, is the pair
/// `just generate` and `just generate-check` run: the write lands at the committed path
/// and the check over the same tree accepts it.
#[test]
fn the_analyzer_manifest_writes_below_its_root_and_checks_clean() -> TestResult {
    let repository = repository_root()?;
    let root = analyzer_root(&repository)?;
    let written = schema::parse_arguments([
        "--analyzer-manifest".to_owned(),
        root.path().display().to_string(),
    ])?;

    schema::run(&written)?;

    let path = root.path().join(rift_index::analyzer_manifest_path());
    let rendered = fs::read_to_string(&path)?;
    assert!(rendered.ends_with('\n'), "{rendered}");
    let document: Value = serde_json::from_str(&rendered)?;
    assert!(
        document["sources"]
            .as_array()
            .is_some_and(|sources| !sources.is_empty()),
        "{document}"
    );

    let checked = schema::parse_arguments([
        "--check".to_owned(),
        "--analyzer-manifest".to_owned(),
        root.path().display().to_string(),
    ])?;
    schema::run(&checked)?;

    fs::write(&path, "{}\n")?;
    let error = schema::run(&checked).expect_err("a stale manifest fails the check");
    let ExportError::CheckMismatch { path: named } = error else {
        panic!("expected CheckMismatch, got {error:?}");
    };
    assert!(named.ends_with("analyzer-manifest.json"));
    Ok(())
}

/// A root whose manifest path is not a writable file: the render succeeds and the write
/// refuses, naming the path it could not write.
#[test]
fn the_analyzer_manifest_names_a_path_it_cannot_write() -> TestResult {
    let repository = repository_root()?;
    let root = analyzer_root(&repository)?;
    fs::create_dir_all(root.path().join(rift_index::analyzer_manifest_path()))?;
    let request = schema::parse_arguments([
        "--analyzer-manifest".to_owned(),
        root.path().display().to_string(),
    ])?;

    let error = schema::run(&request).expect_err("a directory takes no document");

    let ExportError::WriteFailed { path, .. } = error else {
        panic!("expected WriteFailed, got {error:?}");
    };
    assert!(path.ends_with("analyzer-manifest.json"));
    Ok(())
}

/// The analyzer manifest is rendered from the repository tree, so a root holding none of
/// its inputs names the first missing one and the remedy rather than writing a manifest
/// derived from nothing.
#[test]
fn the_analyzer_manifest_over_a_root_without_inputs_names_the_missing_path() -> TestResult {
    let directory = tempfile::tempdir()?;
    let request = schema::parse_arguments([
        "--analyzer-manifest".to_owned(),
        directory.path().display().to_string(),
    ])?;

    let error = schema::run(&request).expect_err("a root holding no inputs renders no manifest");
    let ExportError::AnalyzerManifest { .. } = error else {
        panic!("expected AnalyzerManifest, got {error:?}");
    };
    assert!(error.to_string().contains("Cargo.lock"), "{error}");
    assert!(std::error::Error::source(&error).is_some(), "{error}");
    Ok(())
}

#[test]
fn run_with_unknown_flag_prints_error_and_fails() -> TestResult {
    let directory = tempfile::tempdir()?;
    let output = Command::new(
        std::env::var_os("CARGO_BIN_EXE_rift-schema-export")
            .ok_or("test runner must provide CARGO_BIN_EXE_rift-schema-export")?,
    )
    .arg("--bogus")
    .current_dir(directory.path())
    .output()?;
    assert!(!output.status.success());

    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.starts_with("rift-schema-export: "));
    assert!(stderr.contains("--bogus"));
    Ok(())
}

#[test]
fn run_with_explicit_directory_writes_document_there() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;

    let written = fs::read_to_string(directory.path().join("public/mcp.json"))?;
    assert_eq!(written, schema::schema_document());
    Ok(())
}

#[test]
fn plugin_export_writes_the_generated_manifest_and_skill() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;

    let plugin = directory.path().join("plugin");
    let manifest = fs::read_to_string(plugin.join(".claude-plugin/plugin.json"))?;
    assert_eq!(manifest, skill::plugin_manifest());
    let generated = skill::generate(&schema::tool_listing(), SkillForm::Plugin)?;
    let skill_md = fs::read_to_string(plugin.join("skills/rift/SKILL.md"))?;
    assert_eq!(skill_md, generated.skill_md);
    let tools_md = fs::read_to_string(plugin.join("skills/rift/references/tools.md"))?;
    assert_eq!(tools_md, generated.tools_md);
    Ok(())
}

#[test]
fn check_fails_when_the_plugin_manifest_is_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;
    fs::write(
        directory.path().join("plugin/.claude-plugin/plugin.json"),
        "{}",
    )?;

    let error =
        schema::run(&check_request(&directory)?).expect_err("a stale plugin manifest must fail");
    let ExportError::CheckMismatch { path } = error else {
        panic!("expected CheckMismatch, got {error:?}");
    };
    assert!(path.ends_with("plugin.json"));
    Ok(())
}

#[test]
fn check_succeeds_when_document_matches_served_surface() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;

    schema::run(&check_request(&directory)?)?;
    Ok(())
}

#[test]
fn check_fails_when_document_is_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;
    fs::write(directory.path().join("public/mcp.json"), "{}")?;

    let error =
        schema::run(&check_request(&directory)?).expect_err("stale document must fail check");
    assert!(matches!(error, ExportError::CheckMismatch { .. }));
    assert!(std::error::Error::source(&error).is_none());
    Ok(())
}

#[test]
fn check_fails_when_document_is_missing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let error =
        schema::run(&check_request(&directory)?).expect_err("missing document must fail check");
    assert!(matches!(error, ExportError::CheckUnreadable { .. }));
    assert!(std::error::Error::source(&error).is_some());
    Ok(())
}

#[test]
fn parse_arguments_rejects_unknown_flag() {
    let error =
        schema::parse_arguments(["--bogus".to_owned()]).expect_err("unknown flag must fail");
    let message = error.to_string();
    assert_eq!(error.descriptor().code(), "invalid_request");
    let ExportError::UnknownFlag { argument } = error else {
        panic!("expected UnknownFlag, got {error:?}");
    };
    assert_eq!(argument, "--bogus");
    assert!(message.contains("--bogus"));
    assert!(message.contains("usage: rift-schema-export"));
}

#[test]
fn descriptor_codes_match_registry_per_variant() {
    let extra =
        schema::parse_arguments(["first".to_owned(), "second".to_owned(), "third".to_owned()])
            .expect_err("a third positional argument must fail");
    assert_eq!(extra.descriptor().code(), "invalid_request");
    let missing = ExportError::TemplateToolMissing { name: "search" };
    assert_eq!(missing.descriptor().code(), "install_template_missing_tool");
    assert!(missing.to_string().contains("`search`"));
    assert!(std::error::Error::source(&missing).is_none());
}

#[test]
fn check_unreadable_descriptor_is_storage_failure() -> TestResult {
    let directory = tempfile::tempdir()?;
    let error =
        schema::run(&check_request(&directory)?).expect_err("missing document must fail check");
    assert_eq!(error.descriptor().code(), "storage_failure");
    Ok(())
}

#[test]
fn write_failed_descriptor_is_storage_failure() -> TestResult {
    let directory = tempfile::tempdir()?;
    let blocked = directory.path().join("blocked");
    fs::write(&blocked, "not a directory")?;
    let request = schema::parse_arguments([
        blocked.join("nested").display().to_string(),
        directory.path().join("plugin").display().to_string(),
    ])?;
    let error = schema::run(&request).expect_err("writing under a file must fail");
    assert_eq!(error.descriptor().code(), "storage_failure");
    Ok(())
}

#[test]
fn check_mismatch_descriptor_is_artifact_stale() -> TestResult {
    let directory = tempfile::tempdir()?;
    schema::run(&write_request(&directory)?)?;
    fs::write(directory.path().join("public/mcp.json"), "{}")?;

    let error =
        schema::run(&check_request(&directory)?).expect_err("stale document must fail check");
    assert_eq!(error.descriptor().code(), "artifact_stale");
    Ok(())
}

#[test]
fn parse_arguments_rejects_a_third_positional_argument() {
    let error =
        schema::parse_arguments(["first".to_owned(), "second".to_owned(), "third".to_owned()])
            .expect_err("a third positional argument must fail");
    let message = error.to_string();
    let ExportError::ExtraArgument { argument } = error else {
        panic!("expected ExtraArgument, got {error:?}");
    };
    assert_eq!(argument, "third");
    assert!(message.contains("third"));
    assert!(message.contains("usage: rift-schema-export"));
}

#[test]
fn check_error_messages_name_path_and_regenerate_command() -> TestResult {
    let directory = tempfile::tempdir()?;
    let document_path = directory
        .path()
        .join("public/mcp.json")
        .display()
        .to_string();
    let check = check_request(&directory)?;

    let missing = schema::run(&check).expect_err("missing document must fail check");
    assert!(missing.to_string().contains(&document_path));
    assert!(missing.to_string().contains(REGENERATE_COMMAND));

    schema::run(&write_request(&directory)?)?;
    fs::write(directory.path().join("public/mcp.json"), "{}")?;
    let mismatch = schema::run(&check).expect_err("stale document must fail check");
    assert!(mismatch.to_string().contains(&document_path));
    assert!(mismatch.to_string().contains(REGENERATE_COMMAND));
    Ok(())
}

#[test]
fn write_failed_names_path_and_usage() -> TestResult {
    let directory = tempfile::tempdir()?;
    let blocked = directory.path().join("blocked");
    fs::write(&blocked, "not a directory")?;
    let request = schema::parse_arguments([
        blocked.join("nested").display().to_string(),
        directory.path().join("plugin").display().to_string(),
    ])?;

    let error = schema::run(&request).expect_err("writing under a file must fail");
    assert!(matches!(error, ExportError::WriteFailed { .. }));
    assert!(std::error::Error::source(&error).is_some());
    assert!(error.to_string().contains("usage: rift-schema-export"));
    Ok(())
}

#[test]
fn write_failed_when_document_path_is_a_directory() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::create_dir_all(directory.path().join("public/mcp.json"))?;
    let request = write_request(&directory)?;

    let error = schema::run(&request).expect_err("writing over a directory must fail");
    assert!(matches!(error, ExportError::WriteFailed { .. }));
    assert!(std::error::Error::source(&error).is_some());
    Ok(())
}

#[test]
fn schema_document_is_deterministic_and_sorted_by_name() -> TestResult {
    let first = schema::schema_document();
    let second = schema::schema_document();
    assert_eq!(
        first, second,
        "schema_document must be byte-identical across calls"
    );

    let document: Value = serde_json::from_str(&first)?;
    let tools = document["tools"]
        .as_array()
        .ok_or("tools must be an array")?;
    assert!(!tools.is_empty());

    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().ok_or("tool name must be a string"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tools must be sorted by name");

    for tool in tools {
        let name = tool["name"].as_str().ok_or("tool name must be a string")?;
        let input_schema = &tool["input_schema"];
        let since = input_schema["rift:since"]
            .as_str()
            .ok_or("every tool input model must declare rift:since")?;
        assert!(
            since.starts_with('v'),
            "tool version must use release spelling: name={name}, since={since}"
        );
        let example = input_schema["examples"]
            .as_array()
            .and_then(|examples| examples.first())
            .ok_or("every tool input model must declare an example")?;
        let validator = jsonschema::validator_for(input_schema)?;
        let failures: Vec<String> = validator
            .iter_errors(example)
            .map(|failure| failure.to_string())
            .collect();
        assert!(
            failures.is_empty(),
            "tool input example must validate: name={name}, failures={failures:#?}"
        );
    }
    Ok(())
}

#[test]
fn tool_listing_matches_the_exported_document_and_stays_sorted() -> TestResult {
    let tools = schema::tool_listing();
    assert!(!tools.is_empty());
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tool_listing must be sorted by name");

    let document: Value = serde_json::from_str(&schema::schema_document())?;
    let exported_names: Vec<&str> = document["tools"]
        .as_array()
        .ok_or("tools must be an array")?
        .iter()
        .map(|tool| tool["name"].as_str().ok_or("tool name must be a string"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        names, exported_names,
        "tool_listing must name the same tools schema_document exports"
    );
    Ok(())
}
