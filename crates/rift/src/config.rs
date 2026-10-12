//! Request statistics configuration writes.

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
    /// Set request statistics enablement in this project's rift.toml.
    Set {
        /// Disable request statistics with true, or enable them with false.
        #[arg(long = "telemetry.disabled", action = ArgAction::Set, require_equals = true, value_name = "BOOL", required = true)]
        telemetry_disabled: bool,
        /// Write ~/.rift/config.toml; a user disable applies to every project.
        #[arg(long)]
        user: bool,
    },
}

pub(super) fn run(
    command: &ConfigCommand,
    root: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, RiftError> {
    let ConfigCommand::Set {
        telemetry_disabled,
        user,
    } = command;
    let path = if *user {
        home.ok_or_else(|| {
            errors::cli::config_home_unresolved()
                .checked("HOME, USERPROFILE")
                .error()
        })?
        .join(rift_mcp::telemetry::USER_CONFIGURATION_FILE)
    } else {
        root.join(WORKSPACE_CONFIGURATION_FILE)
    };
    set(&path, *telemetry_disabled)?;
    Ok(path)
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
    let (range, replacement) = replacement(path, &document, disabled)?;
    let mut rendered = raw.to_owned();
    rendered.replace_range(range, &replacement);
    if rendered.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
        return Err(invalid(path, "configuration exceeds the file byte limit"));
    }
    Document::parse(rendered.as_str()).map_err(|error| invalid(path, &error.to_string()))?;
    Ok(rendered)
}

fn replacement(
    path: &Path,
    document: &Document<&str>,
    disabled: bool,
) -> Result<(std::ops::Range<usize>, String), RiftError> {
    let raw = document.raw();
    let Some(telemetry) = document.as_table().get("telemetry") else {
        return Ok(append_table(raw, disabled));
    };
    if let Some(previous) = telemetry
        .as_table_like()
        .and_then(|table| table.get("disabled"))
    {
        let value = previous
            .as_value()
            .filter(|value| value.is_bool())
            .ok_or_else(|| invalid(path, "telemetry.disabled must be a boolean"))?;
        let range = value
            .span()
            .ok_or_else(|| invalid(path, "configuration value has no parsed span"))?;
        return Ok((range, disabled.to_string()));
    }
    match telemetry {
        Item::Table(table) if table.is_dotted() => {
            let ending = line_ending(raw);
            Ok((0..0, format!("telemetry.disabled = {disabled}{ending}")))
        }
        Item::Table(table) => match table.span() {
            Some(header) => {
                let line = lines_inclusive(&raw[header.start..])
                    .next()
                    .unwrap_or_default();
                let end = header.start + line.len();
                let ending = line_ending(raw);
                let prefix = if LineEnding::of(line).is_some() {
                    ""
                } else {
                    ending
                };
                Ok((end..end, format!("{prefix}disabled = {disabled}{ending}")))
            }
            None => Ok(append_table(raw, disabled)),
        },
        Item::Value(Value::InlineTable(_)) => inline_replacement(path, document, disabled),
        _ => Err(invalid(path, "telemetry must be a table")),
    }
}

fn line_ending(raw: &str) -> &'static str {
    lines_inclusive(raw)
        .find_map(LineEnding::of)
        .unwrap_or(LineEnding::Lf)
        .as_str()
}

fn append_table(raw: &str, disabled: bool) -> (std::ops::Range<usize>, String) {
    let ending = line_ending(raw);
    let prefix = if raw.is_empty() || LineEnding::of(raw).is_some() {
        ""
    } else {
        ending
    };
    (
        raw.len()..raw.len(),
        format!("{prefix}[telemetry]{ending}disabled = {disabled}{ending}"),
    )
}

fn inline_replacement(
    path: &Path,
    document: &Document<&str>,
    disabled: bool,
) -> Result<(std::ops::Range<usize>, String), RiftError> {
    let value = document
        .as_table()
        .get("telemetry")
        .and_then(Item::as_value)
        .ok_or_else(|| invalid(path, "telemetry must be a table"))?;
    let range = value
        .span()
        .ok_or_else(|| invalid(path, "configuration value has no parsed span"))?;
    let mut mutable = document.clone().into_mut();
    let table = mutable
        .get_mut("telemetry")
        .and_then(Item::as_inline_table_mut)
        .ok_or_else(|| invalid(path, "telemetry must be a table"))?;
    table.insert("disabled", Value::from(disabled));
    table.decor_mut().clear();
    Ok((range, table.to_string()))
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
                telemetry_disabled: true,
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
        assert!(
            run(
                &ConfigCommand::Set {
                    telemetry_disabled: true,
                    user: true
                },
                root.path(),
                None
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn test_user_config_creates_parent_and_preserves_false_as_local_choice()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let command = ConfigCommand::Set {
            telemetry_disabled: true,
            user: true,
        };
        let path = run(&command, root.path(), Some(home.path()))?;
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
            assert_eq!(accepted["telemetry"]["disabled"].as_bool(), Some(true));
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
}
