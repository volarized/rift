//! Installed language server recommendations from visible project files.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_error::RiftError;
use rift_index::{WorkspaceIndexLimits, WorkspaceSourcePolicy};
use rift_protocol::configuration::{CONFIGURATION_FILE_BYTES_MAX, WorkspaceConfiguration};

pub(super) fn recommend(root: &Path) -> Result<String, RiftError> {
    let root = std::fs::canonicalize(root).map_err(|error| super::write_failure(root, error))?;
    let configuration = rift_server::load_configuration(&root)?;
    let registrations = registration_roots(&root, &configuration)?;
    let mut projects = collect(&root, &configuration)?;
    // Only the primary document registers projects; their own registrations are not followed.
    for project in registrations.values() {
        let configuration = rift_server::load_configuration(project)?;
        let found = collect(project, &configuration)?;
        for ((_, language), origin) in found {
            projects
                .entry((project.clone(), language))
                .or_insert(origin);
        }
    }
    let projects = grouped(&root, projects, &registrations);
    render(&root, projects, &registrations)
}

fn collect(
    root: &Path,
    configuration: &WorkspaceConfiguration,
) -> Result<BTreeMap<(PathBuf, String), PathBuf>, RiftError> {
    let policy = policy(root, configuration)?;
    let paths = policy.visible_paths()?;
    let roots = manifest_roots(&paths);
    let mut projects = BTreeMap::new();
    for path in &paths {
        let Some(language) = policy
            .language_policy()
            .language_for_path(Path::new(path.as_str()))?
        else {
            continue;
        };
        if !language.enabled() {
            continue;
        }
        let identity = language.identity();
        let Some(candidate) = Candidate::for_language(identity) else {
            continue;
        };
        let primary_cargo = candidate.manifest == "Cargo.toml"
            && roots.contains(&("Cargo.toml".to_owned(), PathBuf::new()));
        let project = if primary_cargo {
            PathBuf::new()
        } else {
            nearest_root(Path::new(path.as_str()), candidate.manifest, &roots)
        };
        projects
            .entry((root.join(project), identity.to_owned()))
            .or_insert_with(|| root.join(path.as_str()));
    }
    Ok(projects)
}

fn registration_roots(
    root: &Path,
    configuration: &WorkspaceConfiguration,
) -> Result<BTreeMap<String, PathBuf>, RiftError> {
    let path = root.join("rift.toml");
    let mut roots = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (name, saved) in &configuration.mcp.extra {
        let project = super::profile::registered_root(root, saved, &path)?;
        if project == root || !project.is_dir() || !seen.insert(project.clone()) {
            return Err(super::lsp::invalid(
                &path,
                "registered projects must name distinct folders different from the primary project",
            ));
        }
        roots.insert(name.clone(), project);
    }
    Ok(roots)
}

fn grouped(
    root: &Path,
    projects: BTreeMap<(PathBuf, String), PathBuf>,
    registered: &BTreeMap<String, PathBuf>,
) -> BTreeMap<(PathBuf, String), PathBuf> {
    let candidates: BTreeSet<_> = projects
        .keys()
        .map(|(project, _)| project.clone())
        .collect();
    let mut grouped = BTreeMap::new();
    for ((project, language), origin) in projects {
        let registered_owner = registered
            .values()
            .filter(|owner| project.starts_with(owner))
            .max_by_key(|owner| owner.components().count());
        let proposed_owner = candidates
            .iter()
            .filter(|owner| owner.as_path() != root && project.starts_with(owner))
            .min_by_key(|owner| owner.components().count());
        let selected = registered_owner.or(proposed_owner).unwrap_or(&project);
        let overlaps_registered_child = registered
            .values()
            .any(|child| child != selected && child.starts_with(selected));
        let selected = if overlaps_registered_child && registered_owner.is_none() {
            root
        } else {
            selected
        };
        grouped
            .entry((selected.to_owned(), language))
            .or_insert(origin);
    }
    grouped
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

fn manifest_roots(paths: &[rift_core::ProjectPath]) -> BTreeSet<(String, PathBuf)> {
    paths
        .iter()
        .filter_map(|path| {
            let path = Path::new(path.as_str());
            let name = path.file_name()?.to_str()?;
            if !["Cargo.toml", "package.json", "pyproject.toml"].contains(&name) {
                return None;
            }
            Some((
                name.to_owned(),
                path.parent().unwrap_or_else(|| Path::new("")).to_owned(),
            ))
        })
        .collect()
}

fn nearest_root(path: &Path, manifest: &str, roots: &BTreeSet<(String, PathBuf)>) -> PathBuf {
    path.parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .find(|parent| roots.contains(&(manifest.to_owned(), (*parent).to_owned())))
        .unwrap_or_else(|| Path::new(""))
        .to_owned()
}

struct Candidate {
    program: &'static str,
    manifest: &'static str,
    local: &'static str,
    arguments: &'static [&'static str],
}

impl Candidate {
    fn for_language(language: &str) -> Option<Self> {
        match language {
            "rust" => Some(Self {
                program: "rust-analyzer",
                manifest: "Cargo.toml",
                local: "",
                arguments: &[],
            }),
            "python" => Some(Self {
                program: "pyright-langserver",
                manifest: "pyproject.toml",
                local: if cfg!(windows) {
                    ".venv/Scripts/pyright-langserver.exe"
                } else {
                    ".venv/bin/pyright-langserver"
                },
                arguments: &["--stdio"],
            }),
            "typescript" | "typescript:tsx" | "javascript" => Some(Self {
                program: "typescript-language-server",
                manifest: "package.json",
                local: if cfg!(windows) {
                    "node_modules/.bin/typescript-language-server.cmd"
                } else {
                    "node_modules/.bin/typescript-language-server"
                },
                arguments: &["--stdio"],
            }),
            _ => None,
        }
    }

    fn installed(&self, root: &Path, search: Option<&OsStr>) -> Option<String> {
        if !self.local.is_empty() && which::which_in(self.local, None::<&OsStr>, root).is_ok() {
            return Some(self.local.to_owned());
        }
        which::which_in(self.program, search, root)
            .ok()
            .map(|_| self.program.to_owned())
    }
}

fn render(
    root: &Path,
    projects: BTreeMap<(PathBuf, String), PathBuf>,
    registered: &BTreeMap<String, PathBuf>,
) -> Result<String, RiftError> {
    let mut output = String::new();
    let search = std::env::var_os("PATH");
    let mut registrations = registered
        .iter()
        .map(|(name, project)| (name.clone(), project_name(root, project)))
        .collect();
    for ((project, language), origin) in projects {
        let relative = project_name(root, &project);
        let configuration = rift_server::load_configuration(&project)?;
        if configuration
            .languages
            .get(&language)
            .is_some_and(|entry| !entry.enabled)
        {
            continue;
        }
        let configured = configuration
            .languages
            .get(&language)
            .is_some_and(|entry| entry.lsp.is_some());
        let Some(candidate) = Candidate::for_language(&language) else {
            continue;
        };
        let origin = project_name(root, &origin);
        let _ = writeln!(output, "{language} in {relative}\n  origin: {origin}");
        if configured {
            output.push_str("  language server already configured\n");
        } else if project != root
            && registered.values().any(|other| {
                other != &project && (other.starts_with(&project) || project.starts_with(other))
            })
        {
            output.push_str("  profile roots overlap; use separate project folders before applying a recommendation\n");
        } else if let Some(program) = candidate.installed(&project, search.as_deref()) {
            let profile = if relative == "." {
                None
            } else {
                Some(profile_name(&relative, &mut registrations))
            };
            let _ = writeln!(
                output,
                "  executable: {program}\n  {}",
                command(
                    &language,
                    &relative,
                    profile.as_deref(),
                    &program,
                    candidate.arguments
                )
            );
            if let Some(profile) = profile {
                let _ = writeln!(output, "  scope: local@{profile}");
                if project.starts_with(root) {
                    let _ = writeln!(output, "  primary source excludes this project folder");
                }
            }
        } else {
            let _ = writeln!(
                output,
                "  executable unavailable: {}; install it for this project and rerun rift config recommend",
                candidate.program
            );
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

fn project_name(root: &Path, project: &Path) -> String {
    project
        .strip_prefix(root)
        .map_or_else(|_| project.to_string_lossy().into_owned(), slash)
}

fn command(
    language: &str,
    root: &str,
    profile: Option<&str>,
    program: &str,
    arguments: &[&str],
) -> String {
    let arguments_out = command_arguments(language, root, profile, program, arguments);
    format!(
        "rift config set {}",
        arguments_out
            .iter()
            .map(|argument| quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn command_arguments(
    language: &str,
    root: &str,
    profile: Option<&str>,
    program: &str,
    arguments: &[&str],
) -> Vec<String> {
    let mut arguments_out = vec![
        format!("--language={language}"),
        format!("--lsp-program={program}"),
    ];
    if let Some(profile) = profile {
        arguments_out.push(format!("--profile={profile}"));
        arguments_out.push(format!("--profile-root={root}"));
    }
    arguments_out.extend(
        arguments
            .iter()
            .map(|argument| format!("--lsp-argument={argument}")),
    );
    arguments_out
}

fn profile_name(root: &str, registrations: &mut BTreeMap<String, String>) -> String {
    if let Some((name, _)) = registrations
        .iter()
        .find(|(_, registered)| registered.as_str() == root)
    {
        return name.clone();
    }
    let root_available =
        rift_protocol::identity::valid_registration_name(root) && !registrations.contains_key(root);
    let name = if root_available {
        root.to_owned()
    } else {
        (1..=registrations.len().saturating_add(1))
            .map(|index| format!("project-{index}"))
            .find(|name| !registrations.contains_key(name))
            .unwrap_or_else(|| {
                unreachable!("one more name than registrations always has a free entry")
            })
    };
    registrations.insert(name.clone(), root.to_owned());
    name
}

fn slash(path: &Path) -> String {
    let path = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if path.is_empty() {
        ".".to_owned()
    } else {
        path
    }
}

fn quote(value: &str) -> String {
    if cfg!(windows) {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(output.contains("scope: local@docs"), "{output}");
        assert!(output.contains("scope: local@project-2"), "{output}");
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
        assert_eq!(primary.mcp.extra["docs"], "docs");
        assert_eq!(primary.mcp.extra["project-2"], "docs space");
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
    fn test_recommendation_command_preserves_literal_arguments_and_profile_root() {
        let command = command(
            "typescript:tsx",
            "docs",
            Some("docs"),
            "node_modules/.bin/typescript-language-server",
            &["--stdio", "argument's value"],
        );
        assert!(command.contains("'--profile=docs'"));
        assert!(command.contains("'--profile-root=docs'"));
        assert!(command.contains("'--language=typescript:tsx'"));
        if cfg!(windows) {
            assert!(command.contains("argument''s value"));
        } else {
            assert!(command.contains("argument'\"'\"'s value"));
        }
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
        let arguments = command_arguments(
            "typescript",
            "docs",
            Some("docs"),
            "node_modules/.bin/typescript-language-server",
            &["--stdio", "argument's value"],
        );
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
        assert_eq!(primary.mcp.extra["docs"], "docs");
        assert!(
            rift_server::load_configuration(&root.path().join("docs"))?.languages["typescript"]
                .lsp
                .is_some()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_local_executable_precedes_path_without_running_it()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir()?;
        let local = root.path().join("node_modules/.bin");
        std::fs::create_dir_all(&local)?;
        let executable = local.join("typescript-language-server");
        std::fs::write(&executable, "#!/bin/sh\nexit 91\n")?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        let candidate = Candidate::for_language("typescript").expect("supported language");
        assert_eq!(
            candidate
                .installed(root.path(), Some(OsStr::new("/missing")))
                .as_deref(),
            Some("node_modules/.bin/typescript-language-server")
        );
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o644))?;
        assert_eq!(
            candidate.installed(root.path(), Some(OsStr::new("/missing"))),
            None
        );
        Ok(())
    }
}
