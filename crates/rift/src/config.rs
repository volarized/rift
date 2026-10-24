//! Workspace configuration writes and language server recommendations.

mod editing;
mod lsp;
mod profile;
mod recommendations;

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use clap::{ArgAction, Subcommand};
use rift_core::constants::WORKSPACE_CONFIGURATION_FILE;
use rift_core::line::{LineEnding, lines_inclusive};
use rift_error::{RiftError, errors};
use rift_protocol::configuration::CONFIGURATION_FILE_BYTES_MAX;
use toml_edit::{Document, Item, Value};

#[derive(Debug, Subcommand)]
pub(super) enum ConfigCommand {
    /// Set request statistics or a language server in this project's rift.toml.
    Set {
        /// Disable request statistics with true, or enable them with false.
        #[arg(long = "telemetry.disabled", action = ArgAction::Set, require_equals = true, value_name = "BOOL", required_unless_present = "language", conflicts_with = "language")]
        telemetry_disabled: Option<bool>,
        #[command(flatten)]
        lsp: lsp::LspSetOptions,
        /// Write ~/.rift/config.toml; a user disable applies to every project.
        #[arg(long, conflicts_with = "language")]
        user: bool,
    },
    /// Recommend installed language servers and commands that select them.
    Recommend,
}

#[derive(Debug)]
pub(super) enum ConfigOutcome {
    Written(PathBuf),
    Recommendations(String),
}

impl std::fmt::Display for ConfigOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Written(path) => write!(formatter, "configuration written: {}", path.display()),
            Self::Recommendations(recommendations) => formatter.write_str(recommendations),
        }
    }
}

pub(super) fn run(
    command: &ConfigCommand,
    root: &Path,
    home: Option<&Path>,
) -> Result<ConfigOutcome, RiftError> {
    if matches!(command, ConfigCommand::Recommend) {
        return recommendations::recommend(root).map(ConfigOutcome::Recommendations);
    }
    let ConfigCommand::Set {
        telemetry_disabled,
        lsp,
        user,
    } = command
    else {
        unreachable!("recommendation commands return before configuration writes")
    };
    if lsp.profile.is_some() {
        return profile::set(root, lsp);
    }
    let path = if *user {
        home.filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| {
                errors::cli::config_home_unresolved()
                    .checked("operating system home directory")
                    .error()
            })?
            .join(rift_mcp::telemetry::USER_CONFIGURATION_FILE)
    } else {
        root.join(WORKSPACE_CONFIGURATION_FILE)
    };
    if let Some(disabled) = telemetry_disabled {
        set(&path, *disabled)?;
    } else {
        let raw = read(&path).map_err(|source| write_failure(&path, source))?;
        let rendered = lsp.edit(&path, &raw)?;
        persist(&path, &rendered).map_err(|source| write_failure(&path, source))?;
    }
    Ok(ConfigOutcome::Written(path))
}

fn invalid(path: &Path, detail: &str) -> RiftError {
    errors::core::configuration_invalid()
        .file(path.display().to_string())
        .field("telemetry.disabled")
        .detail(detail)
        .error()
}

fn set(path: &Path, disabled: bool) -> Result<(), RiftError> {
    let raw = read(path).map_err(|source| write_failure(path, source))?;
    let rendered = edit(path, &raw, disabled)?;
    persist(path, &rendered).map_err(|source| write_failure(path, source))
}

fn write_failure(path: &Path, source: std::io::Error) -> RiftError {
    errors::cli::config_write_failed()
        .path(path)
        .source(source)
        .error()
}

fn read(path: &Path) -> std::io::Result<String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::other(
            "configuration path is a symbolic link; use a regular file",
        ));
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let mut raw = String::new();
    file.take(CONFIGURATION_FILE_BYTES_MAX + 1)
        .read_to_string(&mut raw)?;
    Ok(raw)
}

fn persist(path: &Path, rendered: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = fs::metadata(path) {
        staged.as_file().set_permissions(metadata.permissions())?;
    }
    staged.write_all(rendered.as_bytes())?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn edit(path: &Path, raw: &str, disabled: bool) -> Result<String, RiftError> {
    if raw.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
        return Err(invalid(path, "configuration exceeds the file byte limit"));
    }
    let document = Document::parse(raw).map_err(|error| invalid(path, &error.to_string()))?;
    if let Some(telemetry) = document.as_table().get("telemetry") {
        let table = telemetry
            .as_table_like()
            .ok_or_else(|| invalid(path, "telemetry must be a table"))?;
        if table
            .get("disabled")
            .is_some_and(|value| value.as_bool().is_none())
        {
            return Err(invalid(path, "telemetry.disabled must be a boolean"));
        }
    }
    let rendered = editing::set(path, raw, &["telemetry", "disabled"], Value::from(disabled))?;
    if rendered.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
        return Err(invalid(path, "configuration exceeds the file byte limit"));
    }
    let accepted =
        Document::parse(rendered.as_str()).map_err(|error| invalid(path, &error.to_string()))?;
    if accepted
        .as_table()
        .get("telemetry")
        .and_then(Item::as_table_like)
        .and_then(|table| table.get("disabled"))
        .and_then(Item::as_bool)
        != Some(disabled)
    {
        return Err(invalid(
            path,
            "configuration edit did not set telemetry.disabled",
        ));
    }
    Ok(rendered)
}

fn line_ending(raw: &str) -> &'static str {
    lines_inclusive(raw)
        .find_map(LineEnding::of)
        .unwrap_or(LineEnding::Lf)
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_write_is_atomic_and_user_scope_requires_home()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("rift.toml");
        fs::write(&path, "# retained\n[source]\nrespect_gitignore = false\n")?;
        run(
            &ConfigCommand::Set {
                telemetry_disabled: Some(true),
                lsp: lsp::LspSetOptions::default(),
                user: false,
            },
            root.path(),
            None,
        )?;
        assert!(
            fs::read_to_string(&path)?
                .starts_with("# retained\n[source]\nrespect_gitignore = false\n")
        );
        fs::write(&path, "[broken")?;
        assert!(set(&path, true).is_err());
        assert_eq!(fs::read_to_string(&path)?, "[broken");
        for home in [None, Some(Path::new(""))] {
            assert!(
                run(
                    &ConfigCommand::Set {
                        telemetry_disabled: Some(true),
                        lsp: lsp::LspSetOptions::default(),
                        user: true
                    },
                    root.path(),
                    home
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn test_user_config_creates_parent_and_preserves_false_as_local_choice()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let command = ConfigCommand::Set {
            telemetry_disabled: Some(true),
            lsp: lsp::LspSetOptions::default(),
            user: true,
        };
        let ConfigOutcome::Written(path) = run(&command, root.path(), Some(home.path()))? else {
            panic!("set must return the written path");
        };
        assert_eq!(path, home.path().join(".rift/config.toml"));
        assert_eq!(fs::read_dir(home.path().join(".rift"))?.count(), 1);
        assert!(!root.path().join("rift.toml").exists());
        set(&path, false)?;
        assert!(fs::read_to_string(path)?.contains("disabled = false"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_config_write_refuses_a_symbolic_link_without_replacing_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let target = root.path().join("target.toml");
        let path = root.path().join("rift.toml");
        fs::write(&target, "# unchanged\n")?;
        std::os::unix::fs::symlink(&target, &path)?;
        assert!(set(&path, true).is_err());
        assert!(fs::symlink_metadata(&path)?.file_type().is_symlink());
        assert_eq!(fs::read_to_string(&target)?, "# unchanged\n");
        Ok(())
    }

    #[test]
    fn test_config_edit_preserves_other_values_comments_and_inline_tables() {
        for original in [
            "# source\n[source]\nrespect_gitignore = false # keep\n\n[telemetry]\ndisabled = false # choice\n",
            "telemetry = { disabled = false } # choice\n[source]\nrespect_gitignore = false # keep\n",
            "telemetry.disabled = false # choice\n[source]\nrespect_gitignore = false # keep\n",
        ] {
            let changed = edit(Path::new("rift.toml"), original, true).expect("valid config");
            assert!(changed.contains("respect_gitignore = false # keep"));
            assert!(changed.contains("# choice"));
            assert!(changed.contains("disabled = true"));
        }
    }

    #[test]
    fn test_config_edit_preserves_line_endings_order_and_missing_trailing_newline() {
        for ending in ["\n", "\r\n"] {
            let original = format!(
                "# keep{ending}[telemetry]{ending}disabled = false # choice{ending}[source]{ending}respect_gitignore = false"
            );
            let changed = edit(Path::new("rift.toml"), &original, true).expect("accepted TOML");
            assert_eq!(
                changed,
                original.replacen("disabled = false", "disabled = true", 1)
            );
        }
    }

    #[test]
    fn test_config_edit_keeps_interleaved_dotted_keys_and_adds_missing_setting() {
        let original = "source.respect_gitignore = false\nserver.num_workers = 4\nsource.excludes = ['skip']\ntelemetry.disabled = false";
        assert_eq!(
            edit(Path::new("rift.toml"), original, true).expect("accepted document"),
            original.replacen("disabled = false", "disabled = true", 1)
        );
        for original in [
            "[telemetry] # choice",
            "telemetry = {} # choice",
            "[telemetry.child]\nvalue = true\n",
            "telemetry.other = true\n",
        ] {
            let changed = edit(Path::new("rift.toml"), original, true).expect("accepted table");
            let accepted = Document::parse(changed.as_str()).expect("valid rendered TOML");
            assert_eq!(
                accepted
                    .as_table()
                    .get("telemetry")
                    .and_then(Item::as_table_like)
                    .and_then(|table| table.get("disabled"))
                    .and_then(Item::as_bool),
                Some(true),
                "original={original:?}; changed={changed:?}"
            );
            assert!(changed.contains("# choice") || !original.contains("# choice"));
        }
    }

    #[test]
    fn test_config_edit_refuses_malformed_wrong_types_and_oversized_documents() {
        for raw in ["[broken", "telemetry = 1", "[telemetry]\ndisabled = 'true'"] {
            assert!(edit(Path::new("rift.toml"), raw, true).is_err());
        }
        assert!(edit(Path::new("rift.toml"), &"x".repeat(256 * 1024 + 1), true).is_err());
    }

    #[test]
    fn test_config_insertion_preserves_headers_and_missing_final_newline() {
        for ending in ["\n", "\r\n"] {
            let header = format!("[telemetry] # retained{ending}");
            assert_eq!(
                edit(Path::new("rift.toml"), &header, true).expect("accepted table"),
                format!("{header}disabled = true{ending}")
            );
            let original = format!("# retained{ending}[source]{ending}respect_gitignore = false");
            assert_eq!(
                edit(Path::new("rift.toml"), &original, true).expect("accepted document"),
                format!("{original}{ending}[telemetry]{ending}disabled = true{ending}")
            );
        }
    }

    #[test]
    fn test_config_write_refusals_preserve_existing_files() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        let path = root.path().join("rift.toml");
        let limit = usize::try_from(CONFIGURATION_FILE_BYTES_MAX)?;
        let original = format!("#{}", "x".repeat(limit - 1));
        fs::write(&path, &original)?;
        assert!(set(&path, true).is_err());
        assert_eq!(fs::read_to_string(&path)?, original);
        assert!(set(&path.join("config.toml"), true).is_err());
        assert_eq!(fs::read_to_string(&path)?, original);
        Ok(())
    }

    #[test]
    fn test_config_updates_preserve_every_unrelated_byte() {
        let cases = [
            "# disabled = false stays in this comment\ntelemetry.'disabled' = false # change only this value\nsource.excludes = ['# keep', 'disabled = false']\n",
            "telemetry = { # table comment\n  disabled = false, # setting comment\n  keep = { x = 1_000, y = 'literal # text' },\n} # after table\n[source] # table order\nrespect_gitignore = true",
            "# header\n[telemetry] # table comment\n\n'disabled'\t=\tfalse # setting comment\n[source]\nexcludes = [\n  'keep', # array comment\n]\n",
        ];
        for raw in cases {
            for ending in ["\n", "\r\n"] {
                let original = raw.replace('\n', ending);
                let target = if original.contains("telemetry.'disabled' = false") {
                    "telemetry.'disabled' = false"
                } else if original.contains("'disabled'\t=\tfalse") {
                    "'disabled'\t=\tfalse"
                } else {
                    "  disabled = false"
                };
                let expected = original.replacen(target, &target.replace("false", "true"), 1);
                assert_eq!(
                    edit(Path::new("rift.toml"), &original, true).expect("accepted document"),
                    expected
                );
            }
        }
    }

    #[test]
    fn test_missing_inline_setting_keeps_comments_dotted_order_and_values() {
        let cases = [
            (
                "telemetry={} # empty\n",
                "telemetry={ disabled = true } # empty\n",
            ),
            (
                "telemetry = { # empty table comment\n} # after\n",
                "telemetry = { disabled = true # empty table comment\n} # after\n",
            ),
            (
                "telemetry = { z.b = 1, keep = '# literal', z.a = [1, 2], } # after",
                "telemetry = { disabled = true, z.b = 1, keep = '# literal', z.a = [1, 2], } # after",
            ),
            (
                "telemetry = {\n  # existing key comment\n  keep = { x = 1_000, y = 'literal # text' },\n} # after table\n[source]\nrespect_gitignore = true\n",
                "telemetry = { disabled = true,\n  # existing key comment\n  keep = { x = 1_000, y = 'literal # text' },\n} # after table\n[source]\nrespect_gitignore = true\n",
            ),
        ];
        for (original, expected) in cases {
            for ending in ["\n", "\r\n"] {
                assert_eq!(
                    edit(
                        Path::new("rift.toml"),
                        &original.replace('\n', ending),
                        true
                    )
                    .expect("accepted document"),
                    expected.replace('\n', ending)
                );
            }
        }
    }
}
