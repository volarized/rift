//! Typed language server configuration edits.

use std::path::Path;

use clap::Args;
use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_error::{RiftError, errors};
use rift_protocol::configuration::{
    CONFIGURATION_FILE_BYTES_MAX, WorkspaceConfiguration, subtree_pattern,
};
use toml_edit::{Array, Document, InlineTable, Item, TableLike, Value};

#[derive(Debug, Default, Args)]
pub(crate) struct LspSetOptions {
    /// Exact language identity, such as typescript or typescript:tsx.
    #[arg(long)]
    pub(super) language: Option<String>,
    /// Named profile in this project; absence selects the primary project.
    #[arg(long, requires = "language")]
    pub(super) profile: Option<String>,
    /// Workspace-relative folder for a new named profile.
    #[arg(long, requires = "profile")]
    pub(super) profile_root: Option<String>,
    /// Language path pattern within the selected project, repeated for additional patterns.
    #[arg(long, requires = "language")]
    pub(super) include: Vec<String>,
    /// Language path pattern to exclude within the selected project.
    #[arg(long, requires = "language")]
    pub(super) exclude: Vec<String>,
    /// Executable name or relative path, started directly without a shell.
    #[arg(long, requires = "language")]
    pub(super) lsp_program: Option<String>,
    /// Installation layout selecting an installed language server without an executable path.
    #[arg(long, requires = "language", conflicts_with = "lsp_program", value_parser = ["cargo", "bun", "npm", "uv"])]
    pub(super) lsp_package_manager: Option<String>,
    /// Literal process argument, repeated for additional arguments.
    #[arg(long, require_equals = true, requires = "lsp_program")]
    pub(super) lsp_argument: Vec<String>,
}

impl LspSetOptions {
    pub(super) fn edit(&self, path: &Path, raw: &str) -> Result<String, RiftError> {
        self.edit_with_prefix(path, raw, &[])
    }

    pub(super) fn edit_with_prefix(
        &self,
        path: &Path,
        raw: &str,
        prefix: &[&str],
    ) -> Result<String, RiftError> {
        let identity = self.language.as_deref().ok_or_else(|| {
            invalid(
                path,
                "select --language and --lsp-program, or --telemetry.disabled",
            )
        })?;
        let (selector, value) = match (
            self.lsp_program.as_deref(),
            self.lsp_package_manager.as_deref(),
        ) {
            (Some(program), None) => {
                let mut command = Array::new();
                command.push(program);
                for argument in &self.lsp_argument {
                    command.push(argument.as_str());
                }
                ("command", Value::Array(command))
            }
            (None, Some(manager)) if self.lsp_argument.is_empty() => {
                ("package_manager", Value::from(manager))
            }
            _ => {
                return Err(invalid(
                    path,
                    "supply exactly one of --lsp-program and --lsp-package-manager with --language",
                ));
            }
        };
        if raw.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
            return Err(invalid(path, "configuration exceeds the file byte limit"));
        }
        let document = Document::parse(raw).map_err(|error| invalid(path, &error.to_string()))?;
        let mut keys = prefix.to_vec();
        keys.extend(["languages", identity, "lsp"]);
        let selected = super::editing::lookup(&document, &keys);
        let mut rendered = if selected.is_some_and(|item| item.as_value().is_some()) {
            let mut lsp = selected
                .and_then(Item::as_inline_table)
                .cloned()
                .unwrap_or_else(InlineTable::new);
            let table = &mut lsp as &mut dyn TableLike;
            for key in ["command", "package_manager", "embedded"] {
                table.remove(key);
            }
            table.insert(selector, Item::Value(value));
            super::editing::set(path, raw, &keys, Value::InlineTable(lsp))?
        } else {
            let mut rendered = raw.to_owned();
            for key in ["command", "package_manager", "embedded"] {
                if key != selector {
                    let mut removed = keys.clone();
                    removed.push(key);
                    rendered = super::editing::remove(path, &rendered, &removed)?;
                }
            }
            keys.push(selector);
            super::editing::set(path, &rendered, &keys, value)?
        };
        for (key, patterns) in [("include", &self.include), ("exclude", &self.exclude)] {
            if !patterns.is_empty() {
                let mut array = Array::new();
                for pattern in patterns {
                    array.push(pattern.as_str());
                }
                let mut keys = prefix.to_vec();
                keys.extend(["languages", identity, key]);
                rendered = super::editing::set(path, &rendered, &keys, Value::Array(array))?;
            }
        }
        validate(&rendered)?;
        Ok(rendered)
    }
}

pub(super) fn exclude_project(raw: &str, path: &Path, root: &str) -> Result<String, RiftError> {
    let document = Document::parse(raw).map_err(|error| invalid(path, &error.to_string()))?;
    let mut excluded = match super::editing::lookup(&document, &["source", "exclude"]) {
        Some(item) => item
            .as_array()
            .cloned()
            .ok_or_else(|| invalid(path, "source.exclude must be an array"))?,
        None => Array::new(),
    };
    let pattern = subtree_pattern(root);
    if !excluded
        .iter()
        .any(|value| value.as_str() == Some(pattern.as_str()))
    {
        excluded.push(pattern);
    }
    super::editing::set(path, raw, &["source", "exclude"], Value::Array(excluded))
}

fn validate(raw: &str) -> Result<(), RiftError> {
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some(raw),
        &ConfigurationEnvironment::default(),
    )?;
    accepted
        .configuration()
        .validate()
        .map_err(|violation| rift_core::configuration_violation_error(&violation))
}

pub(super) fn invalid(path: &Path, detail: &str) -> RiftError {
    errors::core::configuration_invalid()
        .file(path.display().to_string())
        .field("languages")
        .detail(detail)
        .error()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(profile: Option<&str>) -> LspSetOptions {
        LspSetOptions {
            language: Some("typescript".to_owned()),
            profile: profile.map(str::to_owned),
            profile_root: profile.map(|_| "docs".to_owned()),
            lsp_program: Some("node_modules/.bin/typescript-language-server".to_owned()),
            lsp_argument: vec!["--stdio".to_owned(), "argument with spaces".to_owned()],
            ..LspSetOptions::default()
        }
    }

    #[test]
    fn test_process_selection_replaces_embedded_and_language_patterns() -> Result<(), RiftError> {
        let raw = "# retained\r\n[languages.python]\r\ninclude = ['old/**'] # include\r\nexclude = ['cache/**'] # exclude\r\n[languages.python.lsp]\r\nembedded = 'ty'\r\nrequest_timeout = '7s' # timeout";
        let mut selected = options(None);
        selected.language = Some("python".to_owned());
        selected.lsp_program = Some("pyright-langserver".to_owned());
        selected.include = vec!["src/**/*.py".to_owned()];
        selected.exclude = vec!["src/generated/**".to_owned()];
        let rendered = selected.edit(Path::new("rift.toml"), raw)?;
        assert!(rendered.starts_with("# retained\r\n[languages.python]\r\n"));
        assert!(rendered.contains(" # include\r\n"));
        assert!(rendered.contains(" # exclude\r\n"));
        assert!(rendered.ends_with("request_timeout = '7s' # timeout"));
        assert!(!rendered.contains("embedded"));
        let accepted = accept_configuration::<WorkspaceConfiguration>(
            Some(&rendered),
            &ConfigurationEnvironment::default(),
        )?;
        let entry = &accepted.configuration().languages["python"];
        assert_eq!(
            entry.include.as_ref().map(|patterns| patterns
                .iter()
                .map(|pattern| pattern.0.as_str())
                .collect::<Vec<_>>()),
            Some(vec!["src/**/*.py"])
        );
        assert_eq!(
            entry
                .exclude
                .iter()
                .map(|pattern| pattern.0.as_str())
                .collect::<Vec<_>>(),
            ["src/generated/**"]
        );
        let Some(rift_protocol::configuration::LanguageLspConfiguration::Inline(lsp)) = &entry.lsp
        else {
            panic!("process selection must replace the embedded engine");
        };
        assert!(lsp.command.is_some());
        assert!(lsp.embedded.is_none());
        assert_eq!(
            lsp.request_timeout,
            rift_protocol::configuration::Duration::from_millis(7_000)
        );
        Ok(())
    }

    #[test]
    fn test_lsp_edit_preserves_other_settings_and_literal_arguments() -> Result<(), RiftError> {
        let original = "# retained\n[source]\nrespect_gitignore = false # retained\n\n[languages.typescript]\nlsp = 'old'\n[lsp.old]\ncommand = 'old-server'\n";
        let rendered = options(None).edit(Path::new("rift.toml"), original)?;
        assert!(rendered.contains("respect_gitignore = false # retained"));
        assert!(rendered.contains("command = 'old-server'"));
        let configuration = accept_configuration::<WorkspaceConfiguration>(
            Some(&rendered),
            &ConfigurationEnvironment::default(),
        )?;
        let lsp = configuration.configuration().languages["typescript"]
            .lsp
            .as_ref();
        let Some(rift_protocol::configuration::LanguageLspConfiguration::Inline(lsp)) = lsp else {
            panic!("set must select the supplied process");
        };
        assert_eq!(
            lsp.command
                .as_ref()
                .map(rift_protocol::configuration::CommandInput::arguments),
            Some(["--stdio".to_owned(), "argument with spaces".to_owned()].as_slice())
        );
        Ok(())
    }

    #[test]
    fn test_lsp_edit_accepts_inline_tables_and_keeps_named_process() -> Result<(), RiftError> {
        let original = "languages = { typescript = { lsp = 'fallback' } }\n[lsp.fallback]\ncommand = 'fallback-server'\n";
        let rendered = options(Some("docs")).edit(Path::new("rift.toml"), original)?;
        assert!(rendered.contains("command = 'fallback-server'"));
        assert!(rendered.contains("argument with spaces"));
        validate(&rendered)
    }

    #[test]
    fn test_lsp_edit_refuses_absolute_program_and_oversize() {
        let path = Path::new("rift.toml");
        let mut options = options(Some("docs"));
        options.lsp_program = Some("/bin/server".to_owned());
        assert!(options.edit(path, "").is_err());
        assert!(options.edit(path, &"x".repeat(262_145)).is_err());
        assert!(options.edit(path, "languages = 1").is_err());
    }

    #[test]
    fn test_lsp_edits_keep_crlf_comments_and_missing_final_newline() -> Result<(), RiftError> {
        let raw = "# retained\r\n[source]\r\nrespect_gitignore = false # source\r\n[languages.typescript.lsp]\r\ncommand = ['old'] # command";
        let rendered = options(None).edit(Path::new("rift.toml"), raw)?;
        let expected = raw.replace("['old']", "[\"node_modules/.bin/typescript-language-server\", \"--stdio\", \"argument with spaces\"]");
        assert_eq!(rendered, expected);
        Ok(())
    }

    #[test]
    fn test_inline_process_replacement_retains_environment_and_timeouts() -> Result<(), RiftError> {
        let raw = "[languages.typescript]\nlsp = { command = ['old'], environment = { TRACE = 'literal # value' }, request_timeout = '2s' }\n";
        let rendered = options(None).edit(Path::new("rift.toml"), raw)?;
        let accepted = accept_configuration::<WorkspaceConfiguration>(
            Some(&rendered),
            &ConfigurationEnvironment::default(),
        )?;
        let Some(rift_protocol::configuration::LanguageLspConfiguration::Inline(lsp)) =
            &accepted.configuration().languages["typescript"].lsp
        else {
            panic!("inline process required");
        };
        assert_eq!(lsp.environment["TRACE"], "literal # value");
        assert_eq!(
            lsp.request_timeout,
            rift_protocol::configuration::Duration::from_millis(2_000)
        );
        Ok(())
    }
    #[test]
    fn package_manager_replaces_command_and_preserves_process_settings() -> Result<(), RiftError> {
        let raw = "[languages.typescript]\nlsp = { command = ['old'], environment = { TRACE = 'yes' }, request_timeout = '2s' }\n";
        let mut options = options(None);
        options.lsp_program = None;
        options.lsp_argument.clear();
        options.lsp_package_manager = Some("bun".to_owned());
        let rendered = options.edit(Path::new("rift.toml"), raw)?;
        let accepted = accept_configuration::<WorkspaceConfiguration>(
            Some(&rendered),
            &ConfigurationEnvironment::default(),
        )?;
        let lsp = accepted
            .configuration()
            .resolve_language_lsp("typescript")
            .expect("configured process")
            .configuration;
        assert_eq!(
            lsp.package_manager,
            Some(rift_protocol::configuration::LspPackageManager::Bun)
        );
        assert!(lsp.command.is_none());
        assert_eq!(lsp.environment["TRACE"], "yes");
        assert_eq!(lsp.request_timeout.milliseconds(), 2_000);
        let rendered = self::options(None).edit(Path::new("rift.toml"), &rendered)?;
        assert!(!rendered.contains("package_manager"));
        Ok(())
    }
}
