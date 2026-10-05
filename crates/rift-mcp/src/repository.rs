//! Selects accepted server settings for one workspace and its repository.
//!
//! Repository settings come from the main worktree. A workspace keeps its own process when
//! repository settings cannot be accepted, differ from its settings, or differ from the
//! settings a serving process recorded at startup.

use std::path::{Path, PathBuf};

use rift_history::Repository;
use rift_protocol::canonical::canonical_json;
use rift_protocol::configuration::ServerConfiguration;
use rift_protocol::error::ErrorPhase;
use rift_protocol::lock::ProductIdentity;
use rmcp::ErrorData;
use sha2::{Digest as _, Sha256};

use crate::validation::ConfigurationState;

/// Why one workspace keeps its own server process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceFallback {
    /// The workspace is not inside a repository Rift can open.
    RepositoryUnavailable,
    /// The repository has no readable main worktree.
    MainWorktreeUnavailable,
    /// The main worktree's configuration did not accept.
    AuthorityConfigurationInvalid,
    /// Accepted server tables differ. Entries name keys only.
    ServerSettingsDiffer {
        /// Accepted server keys whose values differ.
        keys: Vec<&'static str>,
    },
    /// Workspace or authority settings differ from settings held by the serving process.
    RunningServerSettingsDiffer,
}

/// Accepted server settings selected for one workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerConfigurationSelection {
    /// Serve through the process whose authority is the main worktree.
    Repository {
        /// Canonical main worktree root that supplied accepted settings.
        authority_root: PathBuf,
        /// Canonical git directory shared by repository worktrees.
        common_directory: PathBuf,
        /// Full accepted `[server]` table.
        server: ServerConfiguration,
        /// SHA-256 of canonical JSON for `server`.
        settings_digest: String,
    },
    /// Serve the workspace through its own process.
    Workspace {
        /// Full accepted `[server]` table.
        server: ServerConfiguration,
        /// SHA-256 of canonical JSON for `server`.
        settings_digest: String,
        /// Why repository serving was not selected.
        fallback: WorkspaceFallback,
    },
}

/// Accepts both configurations and selects one server-settings table.
///
/// `process_settings` names the settings recorded by an existing serving process. A
/// repository selection requires both newly accepted tables to equal those settings.
/// Invalid workspace configuration returns its existing typed read refusal. Invalid
/// repository configuration selects workspace serving, whose settings remain accepted.
///
/// # Errors
///
/// Returns the typed read refusal when the workspace configuration cannot be accepted.
pub fn select_server_configuration(
    workspace_root: &Path,
    process_settings: Option<&ServerConfiguration>,
) -> Result<ServerConfigurationSelection, ErrorData> {
    let workspace_state = ConfigurationState::accept(workspace_root);
    let workspace = workspace_state.accepted(ErrorPhase::Read)?;
    let workspace_server = workspace.server;
    let workspace_selection = |fallback| {
        server_settings_digest(&workspace_server).map(|settings_digest| {
            ServerConfigurationSelection::Workspace {
                settings_digest,
                server: workspace_server.clone(),
                fallback,
            }
        })
    };

    let Ok(repository) = Repository::open(workspace_root) else {
        return workspace_selection(WorkspaceFallback::RepositoryUnavailable);
    };
    let Some(authority_root) = repository.main_worktree_root() else {
        return workspace_selection(WorkspaceFallback::MainWorktreeUnavailable);
    };
    let common_directory = std::fs::canonicalize(repository.common_directory()).ok();
    let Some(common_directory) =
        common_directory.filter(|common| common.is_dir() && std::fs::read_dir(common).is_ok())
    else {
        return workspace_selection(WorkspaceFallback::RepositoryUnavailable);
    };

    let authority_state = ConfigurationState::accept(&authority_root);
    let Ok(authority) = authority_state.accepted(ErrorPhase::Read) else {
        return workspace_selection(WorkspaceFallback::AuthorityConfigurationInvalid);
    };
    let authority_server = authority.server;
    if authority_server != workspace_server {
        let keys = differing_server_keys(&authority_server, &workspace_server);
        rift_tracing::warn!(
            component = "mcp",
            operation = "server.selection",
            authority = %authority_root.display(),
            workspace = %workspace_root.display(),
            keys = ?keys,
            "accepted server settings differ; the workspace uses its own process"
        );
        return workspace_selection(WorkspaceFallback::ServerSettingsDiffer { keys });
    }
    if process_settings
        .is_some_and(|recorded| recorded != &authority_server || recorded != &workspace_server)
    {
        rift_tracing::warn!(
            component = "mcp",
            operation = "server.selection",
            authority = %authority_root.display(),
            workspace = %workspace_root.display(),
            "accepted server settings differ from the serving process; the workspace uses its own process"
        );
        return workspace_selection(WorkspaceFallback::RunningServerSettingsDiffer);
    }

    Ok(ServerConfigurationSelection::Repository {
        settings_digest: server_settings_digest(&authority_server)?,
        authority_root,
        common_directory,
        server: authority_server,
    })
}

/// Returns SHA-256 of the canonical JSON rendering of one server-settings table.
///
/// # Errors
///
/// Returns an internal error if settings cannot be serialized as canonical JSON.
pub fn server_settings_digest(server: &ServerConfiguration) -> Result<String, ErrorData> {
    let canonical = canonical_json(server)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    Ok(format!("{:x}", Sha256::digest(canonical.as_bytes())))
}

/// Discovers the existing common Git directory without accepting server settings.
///
/// Lifecycle commands use this after configuration changes so the existing process remains reachable.
#[must_use]
pub fn discover_common_directory(workspace_root: &Path) -> Option<PathBuf> {
    let repository = Repository::open(workspace_root).ok()?;
    std::fs::canonicalize(repository.common_directory()).ok()
}

/// Returns the repository election directory for one product identity.
///
/// # Errors
///
/// Returns an internal error if identity cannot be serialized as canonical JSON.
pub fn repository_election_directory(
    common_directory: &Path,
    identity: &ProductIdentity,
) -> Result<PathBuf, ErrorData> {
    let canonical = canonical_json(identity)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
    Ok(common_directory
        .join(rift_core::constants::RIFT_STATE_DIRECTORY)
        .join(digest))
}

/// Names the accepted `[server]` keys whose values differ.
fn differing_server_keys(
    authority: &ServerConfiguration,
    workspace: &ServerConfiguration,
) -> Vec<&'static str> {
    let ServerConfiguration {
        num_workers: authority_num_workers,
        workspaces: authority_workspaces,
        worker_queue_timeout: authority_worker_queue_timeout,
        validation_interval: authority_validation_interval,
        version_control_timeout: authority_version_control_timeout,
        idle_timeout: authority_idle_timeout,
        readiness_timeout: authority_readiness_timeout,
        port: authority_port,
        port_range: authority_port_range,
    } = authority;
    let ServerConfiguration {
        num_workers: workspace_num_workers,
        workspaces: workspace_workspaces,
        worker_queue_timeout: workspace_worker_queue_timeout,
        validation_interval: workspace_validation_interval,
        version_control_timeout: workspace_version_control_timeout,
        idle_timeout: workspace_idle_timeout,
        readiness_timeout: workspace_readiness_timeout,
        port: workspace_port,
        port_range: workspace_port_range,
    } = workspace;
    let mut keys = Vec::new();
    if authority_num_workers != workspace_num_workers {
        keys.push("server.num_workers");
    }
    if authority_workspaces != workspace_workspaces {
        keys.push("server.workspaces");
    }
    if authority_worker_queue_timeout != workspace_worker_queue_timeout {
        keys.push("server.worker_queue_timeout");
    }
    if authority_validation_interval != workspace_validation_interval {
        keys.push("server.validation_interval");
    }
    if authority_version_control_timeout != workspace_version_control_timeout {
        keys.push("server.version_control_timeout");
    }
    if authority_idle_timeout != workspace_idle_timeout {
        keys.push("server.idle_timeout");
    }
    if authority_readiness_timeout != workspace_readiness_timeout {
        keys.push("server.readiness_timeout");
    }
    if authority_port != workspace_port {
        keys.push("server.port");
    }
    if authority_port_range != workspace_port_range {
        keys.push("server.port_range");
    }
    keys
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use rift_history::fixture::{commit_all, git, init};
    use rift_protocol::configuration::ServerConfiguration;
    use rift_protocol::lock::ProductIdentity;
    use serde_json::json;

    use super::{
        ServerConfigurationSelection, WorkspaceFallback, repository_election_directory,
        select_server_configuration, server_settings_digest,
    };

    fn repository_fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("repository fixture");
        init(directory.path());
        fs::write(directory.path().join("src.rs"), "pub fn beacon() {}\n").expect("fixture source");
        commit_all(directory.path(), "add source");
        let main = fs::canonicalize(directory.path()).expect("canonical main root");
        (directory, main)
    }

    fn add_linked_worktree(repository: &Path, parent: &Path) -> PathBuf {
        let linked = parent.join("linked");
        git(
            repository,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().expect("temporary path is UTF-8"),
                "HEAD",
            ],
        );
        linked
    }

    fn selected_repository(selection: ServerConfigurationSelection) -> (PathBuf, String) {
        match selection {
            ServerConfigurationSelection::Repository {
                authority_root,
                settings_digest,
                ..
            } => (authority_root, settings_digest),
            ServerConfigurationSelection::Workspace { fallback, .. } => {
                panic!("expected repository selection, got {fallback:?}")
            }
        }
    }

    #[test]
    fn main_and_linked_worktrees_select_same_authority_in_either_order() {
        let (_main, main_root) = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(&main_root, linked_parent.path());
        let linked = selected_repository(
            select_server_configuration(&linked, None).expect("linked selection"),
        );
        let main = selected_repository(
            select_server_configuration(&main_root, None).expect("main selection"),
        );

        assert_eq!(main.0, main_root);
        assert_eq!(linked.0, main_root);
        assert_eq!(main.1, linked.1);
    }

    #[test]
    fn a_missing_main_configuration_uses_accepted_defaults() {
        let (_main, main_root) = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(&main_root, linked_parent.path());
        let configuration = ServerConfiguration::default();

        let selection = select_server_configuration(&linked, Some(&configuration))
            .expect("default settings select");

        assert!(matches!(
            selection,
            ServerConfigurationSelection::Repository { .. }
        ));
    }

    #[test]
    fn invalid_authority_uses_workspace_settings() {
        let (_main, main_root) = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(&main_root, linked_parent.path());
        fs::write(main_root.join("rift.toml"), "unknown = true\n")
            .expect("invalid authority configuration");

        let selection = select_server_configuration(&linked, None).expect("workspace fallback");

        assert!(matches!(
            selection,
            ServerConfigurationSelection::Workspace {
                fallback: WorkspaceFallback::AuthorityConfigurationInvalid,
                ..
            }
        ));
    }

    #[test]
    fn invalid_workspace_keeps_typed_configuration_refusal() {
        let (_main, main_root) = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(&main_root, linked_parent.path());
        fs::write(linked.join("rift.toml"), "unknown = true\n")
            .expect("invalid workspace configuration");

        let error = select_server_configuration(&linked, None).expect_err("workspace refusal");

        let typed = error.data.expect("typed refusal");
        assert_eq!(typed["code"], json!("configuration_invalid"));
        assert_eq!(typed["phase"], json!("read"));
    }

    #[test]
    fn accepted_server_conflicts_name_keys_without_values() {
        let (_main, main_root) = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(&main_root, linked_parent.path());
        fs::write(main_root.join("rift.toml"), "[server]\nnum_workers = 3\n")
            .expect("authority settings");
        fs::write(linked.join("rift.toml"), "[server]\nnum_workers = 5\n")
            .expect("workspace settings");

        let selection = select_server_configuration(&linked, None).expect("workspace fallback");

        assert!(matches!(
            selection,
            ServerConfigurationSelection::Workspace {
                fallback: WorkspaceFallback::ServerSettingsDiffer { ref keys },
                ..
            } if keys == &["server.num_workers"]
        ));
    }

    #[test]
    fn changed_settings_do_not_adopt_existing_process() {
        let (_main, main_root) = repository_fixture();
        let linked_parent = tempfile::tempdir().expect("linked parent");
        let linked = add_linked_worktree(&main_root, linked_parent.path());
        let mut recorded = ServerConfiguration::default();
        recorded.num_workers += 1;

        let selection =
            select_server_configuration(&linked, Some(&recorded)).expect("workspace fallback");

        assert!(matches!(
            selection,
            ServerConfigurationSelection::Workspace {
                fallback: WorkspaceFallback::RunningServerSettingsDiffer,
                ..
            }
        ));
    }

    #[test]
    fn settings_digest_uses_canonical_configuration_json() {
        let configuration = ServerConfiguration::default();

        assert_eq!(
            server_settings_digest(&configuration).expect("configuration digest"),
            server_settings_digest(&configuration).expect("configuration digest")
        );
        let mut changed = configuration.clone();
        changed.num_workers += 1;
        assert_ne!(
            server_settings_digest(&configuration).expect("configuration digest"),
            server_settings_digest(&changed).expect("configuration digest")
        );
    }

    #[test]
    fn repository_election_uses_common_directory_and_product_identity() {
        let common = Path::new("/repositories/sample/.git");
        let first = ProductIdentity {
            version: "0.0.17+build-a".to_owned(),
            schema_digest: "a".repeat(64),
        };
        let second = ProductIdentity {
            schema_digest: "b".repeat(64),
            ..first.clone()
        };

        let first_path = repository_election_directory(common, &first).expect("election path");
        let state_directory = common.join(".rift");
        assert_eq!(first_path.parent(), Some(state_directory.as_path()));
        assert_eq!(
            first_path,
            repository_election_directory(common, &first).expect("election path")
        );
        assert_ne!(
            first_path,
            repository_election_directory(common, &second).expect("election path")
        );
    }
}
