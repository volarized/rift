use std::io;
use std::path::{Path, PathBuf};

use rift_core::constants::RIFT_STATE_DIRECTORY;
use rift_error::{RiftError, errors};
use rift_protocol::canonical::canonical_json;
use rift_protocol::identity::SymbolOwner;
use sha2::{Digest as _, Sha256};

/// Selects storage for one existing project root and its fixed local owner.
///
/// The primary project keeps `.rift`. A named project uses a separate directory
/// keyed by its canonical root and accepted owner.
///
/// # Errors
/// Returns the existing election storage error for an invalid owner or root.
pub fn workspace_state_directory(root: &Path, owner: &SymbolOwner) -> Result<PathBuf, RiftError> {
    let canonical = validated_root(root, owner)?;
    let state = canonical.join(RIFT_STATE_DIRECTORY);
    if matches!(owner, SymbolOwner::Local) {
        return Ok(state);
    }
    Ok(state.join(identity_digest(&canonical, owner)?))
}

/// The full root and owner identity used by independently stored history.
pub(crate) fn workspace_identity(root: &Path, owner: &SymbolOwner) -> Result<String, RiftError> {
    identity_digest(&validated_root(root, owner)?, owner)
}

pub(crate) fn validated_root(root: &Path, owner: &SymbolOwner) -> Result<PathBuf, RiftError> {
    owner.validate().map_err(|error| {
        invalid(
            root,
            io::Error::new(io::ErrorKind::InvalidInput, format!("{error:?}")),
        )
    })?;
    if !matches!(
        owner,
        SymbolOwner::Local | SymbolOwner::NamedLocal { .. } | SymbolOwner::ProfileLocal { .. }
    ) {
        return Err(invalid(
            root,
            io::Error::new(io::ErrorKind::InvalidInput, "owner is not local"),
        ));
    }
    let canonical = std::fs::canonicalize(root).map_err(|error| invalid(root, error))?;
    if !canonical.is_dir() {
        return Err(invalid(
            root,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace root is not a directory",
            ),
        ));
    }
    Ok(canonical)
}

fn identity_digest(root: &Path, owner: &SymbolOwner) -> Result<String, RiftError> {
    let canonical =
        canonical_json(&(root, owner)).map_err(|error| invalid(root, io::Error::other(error)))?;
    Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(canonical.as_bytes())))
}

fn invalid(root: &Path, source: io::Error) -> RiftError {
    errors::mcp::election_storage_failed()
        .operation("validate workspace owner")
        .path(root)
        .source(source)
        .error()
}

#[cfg(test)]
mod tests {
    use super::{workspace_identity, workspace_state_directory};
    use rift_protocol::identity::SymbolOwner;

    #[test]
    fn primary_storage_keeps_its_existing_directory() {
        let root = tempfile::tempdir().expect("workspace");
        assert_eq!(
            workspace_state_directory(root.path(), &SymbolOwner::Local).expect("local storage"),
            root.path()
                .canonicalize()
                .expect("canonical root")
                .join(".rift")
        );
        assert!(!root.path().join(".rift").exists());
    }

    #[test]
    fn named_storage_and_history_bind_both_root_and_owner() {
        let first = tempfile::tempdir().expect("first workspace");
        let second = tempfile::tempdir().expect("second workspace");
        let cloud = SymbolOwner::NamedLocal {
            name: "cloud".to_owned(),
        };
        let other = SymbolOwner::NamedLocal {
            name: "other".to_owned(),
        };
        let state = workspace_state_directory(first.path(), &cloud).expect("named storage");
        assert_ne!(
            state,
            workspace_state_directory(first.path(), &SymbolOwner::Local).expect("primary")
        );
        assert_ne!(
            state,
            workspace_state_directory(first.path(), &other).expect("other owner")
        );
        let identity = workspace_identity(first.path(), &cloud).expect("identity");
        assert_eq!(identity.len(), 64);
        assert!(
            identity
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_ne!(
            identity,
            workspace_identity(second.path(), &cloud).expect("other root")
        );
        assert_eq!(
            identity,
            workspace_identity(&first.path().join("."), &cloud).expect("same root")
        );
        assert!(!state.exists(), "selection must not allocate storage");
    }

    #[test]
    fn profile_storage_is_distinct_and_released_owners_are_refused() {
        let root = tempfile::tempdir().expect("workspace");
        let profile = SymbolOwner::ProfileLocal {
            name: "docs".to_owned(),
        };
        let extra = SymbolOwner::NamedLocal {
            name: "docs".to_owned(),
        };
        assert_ne!(
            workspace_state_directory(root.path(), &profile).expect("profile storage"),
            workspace_state_directory(root.path(), &extra).expect("extra storage"),
        );
        assert_ne!(
            workspace_identity(root.path(), &profile).expect("profile history"),
            workspace_identity(root.path(), &extra).expect("extra history"),
        );
        for owner in [
            SymbolOwner::Runtime {
                runtime: "rust".to_owned(),
                version: "1.98.0".to_owned(),
            },
            SymbolOwner::Package {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "serde".to_owned(),
                version: "1.0.0".to_owned(),
            },
        ] {
            owner.validate().expect("valid released owner");
            let error = workspace_state_directory(root.path(), &owner)
                .expect_err("storage requires a local owner");
            assert_eq!(
                error.slug(),
                rift_error::errors::mcp::election_storage_failed::SLUG
            );
            assert!(
                std::error::Error::source(&error)
                    .expect("storage cause")
                    .to_string()
                    .contains("owner is not local")
            );
        }
        assert!(!root.path().join(".rift").exists());
    }

    #[test]
    fn invalid_owner_or_root_allocates_no_storage() {
        let root = tempfile::tempdir().expect("workspace");
        let missing = root.path().join("missing");
        let invalid = SymbolOwner::NamedLocal {
            name: "../cloud".to_owned(),
        };
        assert!(workspace_state_directory(&missing, &invalid).is_err());
        assert!(workspace_state_directory(&missing, &SymbolOwner::Local).is_err());
        std::fs::write(root.path().join("file"), "source").expect("file");
        assert!(workspace_state_directory(&root.path().join("file"), &SymbolOwner::Local).is_err());
        assert!(!missing.exists());
        assert!(!root.path().join(".rift").exists());
    }
}
