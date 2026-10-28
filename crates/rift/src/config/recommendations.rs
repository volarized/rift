//! Installed language server recommendations from visible project files.

use std::fmt::Write as _;
use std::path::Path;

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_error::RiftError;
use rift_index::{WorkspaceIndexLimits, WorkspaceSourcePolicy};
use rift_protocol::configuration::{CONFIGURATION_FILE_BYTES_MAX, WorkspaceConfiguration};
use rift_server::{LspRecommendation, LspRecommendationInputs, LspRecommendationStatus};

pub(super) fn recommend(root: &Path) -> Result<String, RiftError> {
    let root = std::fs::canonicalize(root).map_err(|error| super::write_failure(root, error))?;
    let configuration = rift_server::load_configuration(&root)?;
    let mut inputs = LspRecommendationInputs::new(&root, configuration.clone())?;
    include_project(&mut inputs, &root, &configuration)?;
    // Only the primary document registers projects; their own registrations are not followed.
    let registrations: Vec<_> = inputs.registered_projects().values().cloned().collect();
    for project in registrations {
        let configuration = rift_server::load_configuration(&project)?;
        include_project(&mut inputs, &project, &configuration)?;
    }
    render(&root, inputs.finish()?)
}

fn include_project(
    inputs: &mut LspRecommendationInputs,
    root: &Path,
    configuration: &WorkspaceConfiguration,
) -> Result<(), RiftError> {
    let policy = policy(root, configuration)?;
    let paths = policy.visible_paths()?;
    inputs.include_project(root, &paths, &policy)
}

fn policy(
    root: &Path,
    configuration: &WorkspaceConfiguration,
) -> Result<WorkspaceSourcePolicy, RiftError> {
    let source = &configuration.source;
    let limits = WorkspaceIndexLimits::default();
    let limits = limits
        .with_workspace_bounds(
            usize::try_from(source.files).unwrap_or(usize::MAX),
            usize::try_from(source.workspace_size.bytes()).unwrap_or(usize::MAX),
            usize::try_from(source.declarations).unwrap_or(usize::MAX),
        )?
        .with_read_bounds(
            usize::try_from(source.directory_depth).unwrap_or(usize::MAX),
            limits.results_max(),
        )?;
    WorkspaceSourcePolicy::build_with_languages(
        root,
        limits,
        &SourceVisibility::from(source),
        &TextFileInclusion::from(configuration),
        &LanguageFileSelections::from(configuration),
    )
}

fn render(root: &Path, recommendations: Vec<LspRecommendation>) -> Result<String, RiftError> {
    let mut output = String::new();
    for recommendation in recommendations {
        let LspRecommendation {
            language,
            root: relative,
            origin,
            status,
        } = recommendation;
        let _ = writeln!(output, "{language} in {relative}\n  origin: {origin}");
        match status {
            LspRecommendationStatus::Configured => {
                output.push_str("  language server already configured\n");
            }
            LspRecommendationStatus::Overlapping => {
                output.push_str("  profile roots overlap; use separate project folders before applying a recommendation\n");
            }
            LspRecommendationStatus::Unavailable { program } => {
                let _ = writeln!(
                    output,
                    "  executable unavailable: {program}; install it for this project and rerun rift config recommend"
                );
            }
            LspRecommendationStatus::Available {
                program,
                command,
                scope,
            } => {
                let _ = writeln!(output, "  executable: {program}\n  {command}");
                match scope {
                    rift_protocol::read::SearchScope::ProfileLocal { name } => {
                        let _ = writeln!(
                            output,
                            "  scope: local:{name}\n  primary source excludes this project folder"
                        );
                    }
                    rift_protocol::read::SearchScope::NamedLocal { name } => {
                        let _ = writeln!(output, "  scope: local@{name}");
                    }
                    _ => {}
                }
            }
        }
        if output.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
            return Err(super::lsp::invalid(
                &root.join("rift.toml"),
                "recommendations exceed the configuration byte limit; narrow source.include and retry",
            ));
        }
    }
    if output.is_empty() {
        output.push_str("no supported language server recommendations for visible project files");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    fn local_executable(
        root: &Path,
        relative: &str,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let executable = root.join(relative);
        std::fs::create_dir_all(
            executable
                .parent()
                .expect("fixture executable has a parent"),
        )?;
        let marker = root.join("engine-executed");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 91\n",
                marker.display()
            ),
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        Ok(marker)
    }

    #[cfg(unix)]
    fn apply_recommendation(root: &Path, command: &str) -> Result<(), Box<dyn std::error::Error>> {
        use clap::Parser as _;
        let parsed = std::process::Command::new("sh")
            .args(["-c", &format!("set -- {command}; printf '%s\\0' \"$@\"")])
            .output()?;
        assert!(parsed.status.success(), "{parsed:?}");
        let arguments = String::from_utf8(parsed.stdout)?;
        let cli =
            crate::Cli::try_parse_from(arguments.split('\0').filter(|value| !value.is_empty()))?;
        let Some(crate::CliCommand::Config { command }) = cli.command else {
            panic!("the recommendation must parse as config set");
        };
        super::super::run(&command, root, None)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_installed_recommendations_apply_profiles_without_launching_engines()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("package.json"), "{}")?;
        std::fs::write(root.path().join("main.ts"), "export const main = 1;")?;
        std::fs::write(root.path().join("pyproject.toml"), "")?;
        std::fs::write(root.path().join("main.py"), "answer = 1\n")?;
        std::fs::create_dir(root.path().join("reserved"))?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[mcp.extra]\nproject-1 = 'reserved'\n",
        )?;
        let mut markers = vec![
            local_executable(root.path(), "node_modules/.bin/typescript-language-server")?,
            local_executable(root.path(), ".venv/bin/pyright-langserver")?,
        ];
        for folder in ["docs", "docs space"] {
            let project = root.path().join(folder);
            std::fs::create_dir(&project)?;
            std::fs::write(project.join("package.json"), "{}")?;
            std::fs::write(project.join("page.ts"), "export const page = 1;")?;
            markers.push(local_executable(
                &project,
                "node_modules/.bin/typescript-language-server",
            )?);
        }
        let output = super::super::run(&super::super::ConfigCommand::Recommend, root.path(), None)?
            .to_string();
        assert!(output.contains("python in ."), "{output}");
        assert!(
            output.contains("executable: .venv/bin/pyright-langserver"),
            "{output}"
        );
        assert!(output.contains("scope: local:docs"), "{output}");
        assert!(output.contains("scope: local:project-1"), "{output}");
        assert!(output.contains("'--profile-root=docs space'"), "{output}");
        let commands: Vec<_> = output
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("rift config set "))
            .collect();
        assert_eq!(commands.len(), 4, "{output}");
        for command in commands {
            apply_recommendation(root.path(), command)?;
        }
        let primary = rift_server::load_configuration(root.path())?;
        assert_eq!(primary.mcp.extra["project-1"], "reserved");
        assert_eq!(primary.profiles["docs"].root, "docs");
        assert_eq!(primary.profiles["project-1"].root, "docs space");
        assert!(primary.languages["python"].lsp.is_some());
        assert!(primary.languages["typescript"].lsp.is_some());
        for folder in ["docs", "docs space"] {
            assert!(
                rift_server::load_configuration(&root.path().join(folder))?.languages["typescript"]
                    .lsp
                    .is_some()
            );
        }
        assert!(markers.iter().all(|marker| !marker.exists()));
        let configured = recommend(root.path())?;
        assert!(configured.contains("already configured"), "{configured}");
        assert!(!configured.contains("rift config set"), "{configured}");
        Ok(())
    }

    #[test]
    fn test_cargo_workspace_recommendation_selects_primary_project()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("crates/app"))?;
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = ['crates/app']\n",
        )?;
        std::fs::write(
            root.path().join("crates/app/Cargo.toml"),
            "[package]\nname = 'app'\nversion = '1.0.0'\n[lib]\npath = 'lib.rs'\n",
        )?;
        std::fs::write(
            root.path().join("crates/app/lib.rs"),
            "pub fn member() {}\n",
        )?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[languages.rust]\nlsp.command = 'selected-server'\n",
        )?;
        let output = recommend(root.path())?;
        assert!(output.contains("rust in .\n"), "{output}");
        assert!(output.contains("origin: crates/app/lib.rs"), "{output}");
        assert!(
            output.contains("language server already configured"),
            "{output}"
        );
        assert!(!output.contains("rust in crates/app"), "{output}");
        assert!(!output.contains("rift config set"), "{output}");
        Ok(())
    }

    #[test]
    fn test_recommendations_use_nearest_manifest_and_existing_language_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("docs"))?;
        std::fs::write(root.path().join("package.json"), "{}")?;
        std::fs::write(root.path().join("main.ts"), "export const main = 1;")?;
        std::fs::write(root.path().join("docs/package.json"), "{}")?;
        std::fs::write(root.path().join("docs/page.tsx"), "export const Page = 1;")?;
        let output = recommend(root.path())?;
        assert!(output.contains("typescript in ."));
        assert!(output.contains("typescript:tsx in docs"));
        assert!(output.contains("origin: docs/page.tsx"));
        std::fs::write(
            root.path().join("rift.toml"),
            "[languages.typescript]\nenabled = false\n",
        )?;
        let output = recommend(root.path())?;
        assert!(!output.contains("typescript in ."));
        assert!(output.contains("typescript:tsx in docs"));
        Ok(())
    }

    #[test]
    fn test_explicit_language_server_selection_has_no_replacement_command()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("main.rs"), "fn main() {}")?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[languages.rust]\nlsp.command = 'selected-server'\n",
        )?;
        let output = recommend(root.path())?;
        assert!(output.contains("already configured"));
        assert!(!output.contains("rift config set"));
        Ok(())
    }

    #[test]
    fn test_registered_projects_scan_own_files_and_disabled_languages()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("docs/api"))?;
        std::fs::write(root.path().join("main.ts"), "export const main = 1;")?;
        std::fs::write(root.path().join("docs/page.ts"), "export const page = 1;")?;
        std::fs::write(root.path().join("docs/api/package.json"), "{}")?;
        std::fs::write(
            root.path().join("docs/api/route.ts"),
            "export const route = 1;",
        )?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[mcp.extra]\ndocs = './docs'\n[source]\nexclude = ['docs/**']\n",
        )?;
        let output = recommend(root.path())?;
        assert!(output.contains("typescript in ."), "{output}");
        assert!(output.contains("typescript in docs"), "{output}");
        assert!(!output.contains("typescript in docs/api"), "{output}");
        assert!(!output.contains("profile roots overlap"), "{output}");
        std::fs::write(
            root.path().join("docs/rift.toml"),
            "[languages.typescript]\nenabled = false\n",
        )?;
        let output = recommend(root.path())?;
        assert!(!output.contains("typescript in docs"), "{output}");
        Ok(())
    }

    #[test]
    fn test_apply_arguments_pass_command_parser_and_configuration_acceptance()
    -> Result<(), Box<dyn std::error::Error>> {
        use clap::Parser as _;
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("docs"))?;
        let arguments = [
            "--language=typescript",
            "--profile=docs",
            "--profile-root=docs",
            "--lsp-program=node_modules/.bin/typescript-language-server",
            "--lsp-argument=--stdio",
            "--lsp-argument=argument's value",
        ]
        .map(str::to_owned);
        let cli = crate::Cli::try_parse_from(
            ["rift".to_owned(), "config".to_owned(), "set".to_owned()]
                .into_iter()
                .chain(arguments),
        )?;
        let Some(crate::CliCommand::Config { command }) = cli.command else {
            panic!("config set command expected");
        };
        super::super::run(&command, root.path(), None)?;
        let primary = rift_server::load_configuration(root.path())?;
        assert_eq!(primary.profiles["docs"].root, "docs");
        assert!(
            rift_server::load_configuration(&root.path().join("docs"))?.languages["typescript"]
                .lsp
                .is_some()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_same_name_extra_and_profile_recommendations_keep_owners_and_write_roots()
    -> Result<(), Box<dyn std::error::Error>> {
        use clap::Parser as _;
        let root = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let extra = external.path().join("extra ' workspace");
        let profile = root.path().join("docs");
        for project in [&profile, &extra] {
            std::fs::create_dir(project)?;
            std::fs::write(project.join("package.json"), "{}")?;
            std::fs::write(project.join("page.ts"), "export const page = 1;")?;
            local_executable(project, "node_modules/.bin/typescript-language-server")?;
        }
        let saved = toml_edit::Value::from(extra.to_string_lossy().as_ref());
        let primary_path = root.path().join("rift.toml");
        let original = format!("[mcp.extra]\ndocs = {saved}\n[profiles.docs]\nroot = 'docs'\n");
        std::fs::write(&primary_path, &original)?;
        let output = recommend(root.path())?;
        assert!(output.contains("scope: local@docs"), "{output}");
        assert!(output.contains("scope: local:docs"), "{output}");
        let command = output
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("cd "))
            .ok_or("extra recommendation missing")?;
        assert!(!command.contains("--profile"), "{command}");
        let parsed = std::process::Command::new("sh")
            .args([
                "-c",
                &format!("rift() {{ pwd; printf '%s\\0' \"$@\"; }}; {command}"),
            ])
            .current_dir(root.path())
            .output()?;
        assert!(parsed.status.success(), "{parsed:?}");
        let parsed = String::from_utf8(parsed.stdout)?;
        let (directory, arguments) = parsed.split_once('\n').ok_or("missing extra cwd")?;
        assert_eq!(
            std::fs::canonicalize(directory)?,
            std::fs::canonicalize(&extra)?
        );
        let cli = crate::Cli::try_parse_from(
            std::iter::once("rift").chain(arguments.split('\0').filter(|value| !value.is_empty())),
        )?;
        let Some(crate::CliCommand::Config { command }) = cli.command else {
            panic!("extra command must parse as config set");
        };
        super::super::run(&command, std::path::Path::new(directory), None)?;
        assert_eq!(std::fs::read_to_string(primary_path)?, original);
        assert!(!profile.join("rift.toml").exists());
        assert!(
            rift_server::load_configuration(&extra)?.languages["typescript"]
                .lsp
                .is_some()
        );
        assert!(
            rift_server::load_configuration(root.path())?.profiles["docs"]
                .languages
                .is_empty()
        );
        Ok(())
    }
}
