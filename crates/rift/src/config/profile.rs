//! Profile configuration writes in the repository's root file.

use std::fs;
use std::path::{Component, Path};

use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_error::RiftError;
use rift_protocol::configuration::WorkspaceConfiguration;
use toml_edit::Value;

use super::lsp::LspSetOptions;
use super::{ConfigOutcome, persist, read, write_failure};

pub(super) fn set(root: &Path, options: &LspSetOptions) -> Result<ConfigOutcome, RiftError> {
    let path = root.join(rift_core::constants::WORKSPACE_CONFIGURATION_FILE);
    let original = read(&path).map_err(|error| write_failure(&path, error))?;
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some(&original),
        &ConfigurationEnvironment::default(),
    )?;
    let name = options
        .profile
        .as_deref()
        .ok_or_else(|| super::lsp::invalid(&path, "supply --profile for a named profile"))?;
    let previous = accepted
        .configuration()
        .profiles
        .get(name)
        .map(|profile| profile.root.as_str());
    let selected = match (options.profile_root.as_deref(), previous) {
        (Some(selected), Some(previous)) => {
            let selected = project_root(root, selected, &path)?;
            let previous_root = project_root(root, previous, &path)?;
            if selected != previous_root {
                return Err(super::lsp::invalid(
                    &path,
                    "profile already names another root; keep its root or select another profile name",
                ));
            }
            selected
        }
        (Some(selected), None) => project_root(root, selected, &path)?,
        (None, Some(previous)) => project_root(root, previous, &path)?,
        (None, None) => {
            return Err(super::lsp::invalid(
                &path,
                "supply --profile-root for a new profile",
            ));
        }
    };
    let registered = super::editing::set(
        &path,
        &original,
        &["profiles", name, "root"],
        Value::from(selected.as_str()),
    )?;
    let rendered = options.edit_with_prefix(&path, &registered, &["profiles", name])?;
    let rendered = super::lsp::exclude_project(&rendered, &path, &selected)?;
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some(&rendered),
        &ConfigurationEnvironment::default(),
    )?;
    accepted
        .configuration()
        .validate()
        .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
    rift_server::profile_roots(root, accepted.configuration())?;
    persist(&path, &rendered).map_err(|error| write_failure(&path, error))?;
    Ok(ConfigOutcome::Written(path))
}

fn project_root(root: &Path, relative: &str, path: &Path) -> Result<String, RiftError> {
    let valid = !relative.is_empty()
        && !relative.contains(['\\', ':'])
        && Path::new(relative)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !valid {
        return Err(super::lsp::invalid(
            path,
            "profile root must name a folder below this project, such as docs",
        ));
    }
    let primary = fs::canonicalize(root).map_err(|error| write_failure(path, error))?;
    let project =
        fs::canonicalize(primary.join(relative)).map_err(|error| write_failure(path, error))?;
    if project == primary || !project.starts_with(&primary) || !project.is_dir() {
        return Err(super::lsp::invalid(
            path,
            "profile root must stay below this project",
        ));
    }
    Ok(project
        .strip_prefix(&primary)
        .expect("profile is below primary root")
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> LspSetOptions {
        LspSetOptions {
            language: Some("typescript".to_owned()),
            profile: Some("docs".to_owned()),
            profile_root: Some("docs".to_owned()),
            lsp_program: Some("typescript-language-server".to_owned()),
            lsp_argument: vec!["--stdio".to_owned()],
            ..LspSetOptions::default()
        }
    }

    #[test]
    fn test_profile_writes_root_file_and_preserves_extra_workspaces()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("docs"))?;
        let path = root.path().join("rift.toml");
        fs::write(
            &path,
            "# retained\n[mcp.extra]\ncloud = '../cloud'\n[languages.rust]\nlsp.command = 'rust-analyzer'\n",
        )?;
        set(root.path(), &options())?;
        let primary = rift_server::load_configuration(root.path())?;
        let docs = rift_server::load_configuration(&root.path().join("docs"))?;
        assert!(fs::read_to_string(&path)?.starts_with("# retained\n"));
        assert_eq!(primary.profiles["docs"].root, "docs");
        assert_eq!(primary.mcp.extra["cloud"], "../cloud");
        assert!(!primary.mcp.extra.contains_key("docs"));
        assert!(
            primary
                .source
                .exclude
                .iter()
                .any(|pattern| pattern.0 == "docs/**")
        );
        assert!(primary.languages["rust"].lsp.is_some());
        assert!(docs.languages["typescript"].lsp.is_some());
        assert!(docs.source.exclude.is_empty());
        assert!(!root.path().join("docs/rift.toml").exists());
        let mut next = options();
        next.profile_root = None;
        set(root.path(), &next)?;
        next.profile_root = Some("./docs/".to_owned());
        set(root.path(), &next)?;
        assert_eq!(
            rift_server::load_configuration(root.path())?.profiles["docs"].root,
            "docs"
        );
        Ok(())
    }

    #[test]
    fn test_profile_refuses_invalid_roots_overlap_and_reassignment_without_writes()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("docs/api"))?;
        fs::create_dir(root.path().join("other"))?;
        let mut unnamed_root = options();
        unnamed_root.profile_root = None;
        assert!(set(root.path(), &unnamed_root).is_err());
        assert!(!root.path().join("rift.toml").exists());
        for relative in ["", ".", "../outside", "/tmp", "docs\\outside", "missing"] {
            let mut options = options();
            options.profile_root = Some(relative.to_owned());
            assert!(set(root.path(), &options).is_err());
            assert!(!root.path().join("rift.toml").exists());
        }
        set(root.path(), &options())?;
        let original = fs::read_to_string(root.path().join("rift.toml"))?;
        for (name, relative) in [("api", "docs/api"), ("docs", "other")] {
            let mut options = options();
            options.profile = Some(name.to_owned());
            options.profile_root = Some(relative.to_owned());
            assert!(set(root.path(), &options).is_err());
            assert_eq!(fs::read_to_string(root.path().join("rift.toml"))?, original);
        }
        Ok(())
    }

    #[test]
    fn test_profile_normalizes_root_and_escapes_literal_exclusion()
    -> Result<(), Box<dyn std::error::Error>> {
        use rift_core::{SourceVisibility, TextFileInclusion};
        use rift_index::{WorkspaceIndexLimits, WorkspaceSourcePolicy};
        let folders = ["docs[1]", "#docs", "!docs", "docs{a,b}", "docs space"];
        #[cfg(unix)]
        let folders = folders.into_iter().chain(["docs*", "docs?"]);
        for folder in folders {
            let root = tempfile::tempdir()?;
            fs::create_dir(root.path().join(folder))?;
            fs::write(
                root.path().join(folder).join("page.ts"),
                "export const page = 1;",
            )?;
            fs::create_dir(root.path().join("docs1"))?;
            fs::write(root.path().join("docs1/page.ts"), "export const other = 1;")?;
            let mut options = options();
            options.profile_root = Some(format!("./{folder}//"));
            set(root.path(), &options)?;
            let accepted = rift_server::load_configuration(root.path())?;
            assert_eq!(accepted.profiles["docs"].root, folder);
            let policy = WorkspaceSourcePolicy::build(
                root.path(),
                WorkspaceIndexLimits::default(),
                &SourceVisibility::from(&accepted.source),
                &TextFileInclusion::default(),
            )?;
            let paths = policy.visible_paths()?;
            assert!(
                !paths
                    .iter()
                    .any(|path| path.as_str() == format!("{folder}/page.ts"))
            );
            assert!(paths.iter().any(|path| path.as_str() == "docs1/page.ts"));
        }
        Ok(())
    }
}
