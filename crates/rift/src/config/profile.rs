//! Named local project configuration writes.

use std::fs;
use std::path::{Component, Path};

use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_error::RiftError;
use rift_protocol::configuration::WorkspaceConfiguration;

use super::lsp::LspSetOptions;
use super::{ConfigOutcome, persist, read, write_failure};

pub(super) fn set(root: &Path, options: &LspSetOptions) -> Result<ConfigOutcome, RiftError> {
    set_with(root, options, persist)
}

/// Prepares both files, publishes the child, then makes its registration visible.
/// A failed registration restores the child only while its bytes still match our write.
fn set_with(
    root: &Path,
    options: &LspSetOptions,
    mut publish: impl FnMut(&Path, &str) -> std::io::Result<()>,
) -> Result<ConfigOutcome, RiftError> {
    let registration = root.join(rift_core::constants::WORKSPACE_CONFIGURATION_FILE);
    let original = read(&registration).map_err(|error| write_failure(&registration, error))?;
    let (selected, existing) = selected_root(root, options, &original, &registration)?;
    let (project, relative) = project_root(root, &selected, existing, &registration)?;
    refuse_overlap(root, options, &original, &project, &registration)?;
    let configuration = project.join(rift_core::constants::WORKSPACE_CONFIGURATION_FILE);
    let existed = configuration.exists();
    let previous = read(&configuration).map_err(|error| write_failure(&configuration, error))?;
    let permissions = fs::metadata(&configuration)
        .ok()
        .map(|metadata| metadata.permissions());
    let rendered = options.edit(&configuration, &previous)?;
    let registered = options.register(
        &registration,
        &original,
        relative.as_deref().unwrap_or(&selected),
        relative.as_deref(),
    )?;
    publish(&configuration, &rendered).map_err(|error| write_failure(&configuration, error))?;
    if let Err(error) = publish(&registration, &registered) {
        restore(&configuration, &rendered, &previous, existed, permissions)?;
        return Err(write_failure(&registration, error));
    }
    Ok(ConfigOutcome::Profile {
        registration,
        configuration,
    })
}

fn selected_root(
    root: &Path,
    options: &LspSetOptions,
    raw: &str,
    path: &Path,
) -> Result<(String, bool), RiftError> {
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some(raw),
        &ConfigurationEnvironment::default(),
    )?;
    let previous = options
        .profile
        .as_ref()
        .and_then(|name| accepted.configuration().mcp.extra.get(name));
    match (&options.profile_root, previous) {
        (Some(selected), Some(previous)) => {
            let selected_path = registered_root(root, selected, path)?;
            let previous_path = registered_root(root, previous, path)?;
            if selected_path != previous_path {
                return Err(super::lsp::invalid(
                    path,
                    "profile already names another root; keep its root or select another profile name",
                ));
            }
            Ok((previous.clone(), true))
        }
        (Some(root), _) => Ok((root.clone(), false)),
        (None, Some(previous)) => Ok((previous.clone(), true)),
        (None, None) => Err(super::lsp::invalid(
            Path::new("rift.toml"),
            "supply --profile-root for a new named local project",
        )),
    }
}

fn project_root(
    root: &Path,
    relative: &str,
    existing: bool,
    path: &Path,
) -> Result<(std::path::PathBuf, Option<String>), RiftError> {
    let accepted = !relative.is_empty()
        && Path::new(relative)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
        && !relative.contains('\\');
    if !existing && !accepted {
        return Err(super::lsp::invalid(
            path,
            "profile root must name a folder below this project, such as docs",
        ));
    }
    let primary = fs::canonicalize(root).map_err(|error| write_failure(path, error))?;
    let project = registered_root(&primary, relative, path)?;
    let outside_new_profile = !existing && !project.starts_with(&primary);
    if outside_new_profile || project == primary || !project.is_dir() {
        return Err(super::lsp::invalid(
            path,
            "profile root must stay below this project",
        ));
    }
    let relative = project.strip_prefix(&primary).ok().map(|relative| {
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    });
    Ok((project, relative))
}

fn refuse_overlap(
    root: &Path,
    options: &LspSetOptions,
    raw: &str,
    selected: &Path,
    path: &Path,
) -> Result<(), RiftError> {
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some(raw),
        &ConfigurationEnvironment::default(),
    )?;
    for (name, registered) in &accepted.configuration().mcp.extra {
        if options.profile.as_ref() == Some(name) {
            continue;
        }
        let registered = registered_root(root, registered, path)?;
        if registered.starts_with(selected) || selected.starts_with(&registered) {
            return Err(super::lsp::invalid(
                path,
                "profile roots overlap; select the existing profile or use a separate project folder",
            ));
        }
    }
    Ok(())
}

pub(super) fn registered_root(
    root: &Path,
    saved: &str,
    path: &Path,
) -> Result<std::path::PathBuf, RiftError> {
    let selected = if saved == "~" || saved.starts_with("~/") {
        let home = std::env::home_dir().ok_or_else(|| {
            super::lsp::invalid(
                path,
                "home directory is unavailable; use a project path without home expansion",
            )
        })?;
        home.join(saved.strip_prefix("~/").unwrap_or(""))
    } else {
        root.join(saved)
    };
    fs::canonicalize(selected).map_err(|error| write_failure(path, error))
}

fn restore(
    path: &Path,
    rendered: &str,
    previous: &str,
    existed: bool,
    permissions: Option<fs::Permissions>,
) -> Result<(), RiftError> {
    let current = read(path).map_err(|error| write_failure(path, error))?;
    if current != rendered {
        return Err(super::lsp::invalid(
            path,
            "registration failed and project configuration changed; inspect both configuration files before retrying",
        ));
    }
    if existed {
        persist(path, previous).map_err(|error| write_failure(path, error))?;
        if let Some(permissions) = permissions {
            fs::set_permissions(path, permissions).map_err(|error| write_failure(path, error))?;
        }
    } else {
        fs::remove_file(path).map_err(|error| write_failure(path, error))?;
    }
    Ok(())
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
    fn test_profile_registration_uses_existing_named_project_configuration()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("docs"))?;
        let primary = root.path().join("rift.toml");
        fs::write(
            &primary,
            "# retained\n[languages.rust]\nlsp.command = 'rust-analyzer'\n",
        )?;
        set(root.path(), &options())?;
        let primary = rift_server::load_configuration(root.path())?;
        let docs = rift_server::load_configuration(&root.path().join("docs"))?;
        assert_eq!(primary.mcp.extra["docs"], "docs");
        assert!(
            primary
                .source
                .exclude
                .iter()
                .any(|pattern| pattern.0 == "docs/**")
        );
        assert!(primary.languages["rust"].lsp.is_some());
        assert!(docs.languages["typescript"].lsp.is_some());
        assert!(!primary.languages.contains_key("typescript"));
        Ok(())
    }

    #[test]
    fn test_failed_registration_restores_original_child_or_removes_new_child()
    -> Result<(), Box<dyn std::error::Error>> {
        for existing in [false, true] {
            let root = tempfile::tempdir()?;
            fs::create_dir(root.path().join("docs"))?;
            let child = root.path().join("docs/rift.toml");
            if existing {
                fs::write(&child, "# existing\n")?;
            }
            let failure = set_with(root.path(), &options(), |path, raw| {
                if path == root.path().join("rift.toml") {
                    Err(std::io::Error::other("held registration"))
                } else {
                    persist(path, raw)
                }
            });
            assert!(failure.is_err());
            if existing {
                assert_eq!(fs::read_to_string(&child)?, "# existing\n");
            } else {
                assert!(!child.exists());
            }
            assert!(!root.path().join("rift.toml").exists());
        }
        Ok(())
    }

    #[test]
    fn test_failed_registration_preserves_concurrent_child_edit()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("docs"))?;
        let primary = root.path().join("rift.toml");
        let child = root.path().join("docs/rift.toml");
        fs::write(&primary, "# primary\n")?;
        fs::write(&child, "# original\n")?;
        let result = set_with(root.path(), &options(), |path, raw| {
            if path == primary {
                fs::write(&child, "# concurrent edit\n")?;
                Err(std::io::Error::other("held registration"))
            } else {
                persist(path, raw)
            }
        });
        let error = result.expect_err("registration must refuse after the child changed");
        assert!(
            error.to_string().contains("configuration changed"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&child)?, "# concurrent edit\n");
        assert_eq!(fs::read_to_string(&primary)?, "# primary\n");
        Ok(())
    }

    #[test]
    fn test_profile_refuses_escaping_root_without_writes() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        for relative in [".", "../outside", "/tmp", "docs\\outside"] {
            let mut options = options();
            options.profile_root = Some(relative.to_owned());
            assert!(set(root.path(), &options).is_err());
            assert!(!root.path().join("rift.toml").exists());
        }
        Ok(())
    }

    #[test]
    fn test_profile_normalizes_root_and_escapes_literal_exclusion()
    -> Result<(), Box<dyn std::error::Error>> {
        use rift_core::{SourceVisibility, TextFileInclusion};
        use rift_index::{WorkspaceIndexLimits, WorkspaceSourcePolicy};
        for folder in ["docs[1]", "#docs", "!docs", "docs{a,b}", "docs space"] {
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
            assert_eq!(accepted.mcp.extra["docs"], folder);
            let visibility = SourceVisibility::from(&accepted.source);
            let policy = WorkspaceSourcePolicy::build(
                root.path(),
                WorkspaceIndexLimits::default(),
                &visibility,
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

    #[test]
    fn test_profile_refuses_overlap_and_reassignment_without_writes()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("docs/api"))?;
        fs::create_dir(root.path().join("other"))?;
        set(root.path(), &options())?;
        let original = fs::read_to_string(root.path().join("rift.toml"))?;
        let mut nested = options();
        nested.profile = Some("api".to_owned());
        nested.profile_root = Some("docs/api".to_owned());
        assert!(set(root.path(), &nested).is_err());
        assert!(!root.path().join("docs/api/rift.toml").exists());
        let mut reassigned = options();
        reassigned.profile_root = Some("other".to_owned());
        assert!(set(root.path(), &reassigned).is_err());
        assert!(!root.path().join("other/rift.toml").exists());
        assert_eq!(fs::read_to_string(root.path().join("rift.toml"))?, original);
        Ok(())
    }

    #[test]
    fn test_existing_external_profile_keeps_primary_visibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let path = toml_edit::Value::from(external.path().to_string_lossy().as_ref());
        fs::write(
            root.path().join("rift.toml"),
            format!("[mcp.extra]\nexternal = {path}\n"),
        )?;
        let mut options = options();
        options.profile = Some("external".to_owned());
        options.profile_root = None;
        set(root.path(), &options)?;
        let primary = rift_server::load_configuration(root.path())?;
        assert!(primary.source.exclude.is_empty());
        assert!(external.path().join("rift.toml").is_file());
        Ok(())
    }
}
