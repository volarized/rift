//! Exact installed Cargo roots established by captured lock entries and metadata.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use rift_protocol::read::SourceLocation;
use serde::Deserialize;

use super::CargoResolver;
use crate::{DependencyContext, InstallFolder, InstallLocation};

/// Why a captured Cargo metadata answer cannot establish installed source roots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CargoMetadataError {
    /// The answer exceeds the accepted byte or package count bound.
    Bound,
    /// JSON fields or metadata format version are invalid.
    InvalidFormat,
    /// The metadata workspace differs from the selected workspace.
    RootMismatch,
    /// Two selected owners claim the same physical source root or Source ID.
    ConflictingOwner,
}

impl fmt::Display for CargoMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bound => "Cargo metadata exceeds the accepted byte or package bound",
            Self::InvalidFormat => "Cargo metadata has invalid fields or format version",
            Self::RootMismatch => "Cargo metadata belongs to another workspace root",
            Self::ConflictingOwner => "Cargo metadata source roots have conflicting package owners",
        })
    }
}

impl std::error::Error for CargoMetadataError {}

#[derive(Deserialize)]
struct Metadata {
    version: u64,
    workspace_root: PathBuf,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: PathBuf,
}

impl CargoResolver {
    /// Joins metadata v1 with the original Source IDs of accepted lock entries.
    ///
    /// Source IDs are opaque. Neither registry folder hashes nor package basenames
    /// establish ownership. Only matching source, name and exact version admit the
    /// absolute manifest root reported by Cargo for the selected workspace.
    ///
    /// # Errors
    /// Refuses malformed, oversized, foreign-root or conflicting metadata.
    pub fn installed_folders(
        bytes: &[u8],
        workspace: &Path,
        accepted: &DependencyContext,
        bytes_max: usize,
        packages_max: usize,
    ) -> Result<Vec<InstallFolder>, CargoMetadataError> {
        if bytes.len() > bytes_max {
            return Err(CargoMetadataError::Bound);
        }
        let metadata: Metadata =
            serde_json::from_slice(bytes).map_err(|_| CargoMetadataError::InvalidFormat)?;
        if metadata.version != 1 {
            return Err(CargoMetadataError::InvalidFormat);
        }
        if metadata.workspace_root != workspace {
            return Err(CargoMetadataError::RootMismatch);
        }
        if metadata.packages.len() > packages_max {
            return Err(CargoMetadataError::Bound);
        }
        let mut selected = BTreeMap::new();
        for folder in accepted.install_folders() {
            let (
                SourceLocation::Dependency { package },
                InstallLocation::CargoRegistry { source, .. },
            ) = (&folder.origin, &folder.location)
            else {
                continue;
            };
            let key = (
                source.as_str(),
                package.name.as_str(),
                package.version.as_str(),
            );
            if selected
                .insert(key, &folder.origin)
                .is_some_and(|previous| previous != &folder.origin)
            {
                return Err(CargoMetadataError::ConflictingOwner);
            }
        }
        let mut roots = BTreeMap::new();
        let mut installed = BTreeMap::new();
        for package in &metadata.packages {
            let Some(source) = package.source.as_deref() else {
                continue;
            };
            let Some(origin) =
                selected.get(&(source, package.name.as_str(), package.version.as_str()))
            else {
                continue;
            };
            if !package.manifest_path.is_absolute()
                || package
                    .manifest_path
                    .file_name()
                    .is_none_or(|name| name != "Cargo.toml")
            {
                return Err(CargoMetadataError::InvalidFormat);
            }
            let root = package
                .manifest_path
                .parent()
                .ok_or(CargoMetadataError::InvalidFormat)?
                .to_path_buf();
            let key = (source, package.name.as_str(), package.version.as_str());
            if installed
                .insert(key, root.clone())
                .is_some_and(|previous| previous != root)
            {
                return Err(CargoMetadataError::ConflictingOwner);
            }
            let origin: SourceLocation = (**origin).clone();
            if roots
                .insert(root, origin.clone())
                .is_some_and(|previous| previous != origin)
            {
                return Err(CargoMetadataError::ConflictingOwner);
            }
        }
        Ok(roots
            .into_iter()
            .map(|(root, origin)| InstallFolder {
                origin,
                location: InstallLocation::Path(root),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::RecordedInspector;
    use rift_protocol::read::ProjectPath;

    const SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

    fn accepted(root: &Path) -> DependencyContext {
        let mut inputs = RecordedInspector::default().with_file(root.join("Cargo.lock"),
            format!("version = 4\n[[package]]\nname = 'helper'\nversion = '1.0.0'\nsource = '{SOURCE}'\n"));
        crate::resolve_context(
            root,
            &[ProjectPath("Cargo.toml".to_owned())],
            &[&CargoResolver::new()],
            &mut inputs,
            &[],
        )
    }

    fn metadata(root: &Path, source: &serde_json::Value, manifest: &Path) -> Vec<u8> {
        serde_json::to_vec(
            &serde_json::json!({"version":1,"workspace_root":root,"packages":[{
                "name":"helper","version":"1.0.0","source":source,"manifest_path":manifest
            }]}),
        )
        .expect("metadata fixture")
    }

    #[test]
    fn test_metadata_admits_the_exact_lock_source_at_cargos_manifest_root() {
        let workspace = std::env::temp_dir().join("rift-cargo-metadata-workspace");
        let root = std::env::temp_dir().join("registry/opaque-directory/helper");
        let bytes = metadata(
            &workspace,
            &serde_json::json!(SOURCE),
            &root.join("Cargo.toml"),
        );
        let folders =
            CargoResolver::installed_folders(&bytes, &workspace, &accepted(&workspace), 4_096, 10)
                .expect("exact association");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].location, InstallLocation::Path(root));
        let SourceLocation::Dependency { package } = &folders[0].origin else {
            panic!("package owner");
        };
        assert_eq!(package.registry, "crates.io");
        assert_eq!(
            (&package.name, &package.version),
            (&"helper".to_owned(), &"1.0.0".to_owned())
        );
    }

    #[test]
    fn test_metadata_does_not_infer_a_missing_or_different_source() {
        let workspace = std::env::temp_dir().join("rift-cargo-metadata-workspace");
        let manifest = std::env::temp_dir().join("registry/helper/Cargo.toml");
        for source in [
            serde_json::Value::Null,
            serde_json::json!("registry+https://other.example/index"),
        ] {
            let bytes = metadata(&workspace, &source, &manifest);
            assert!(
                CargoResolver::installed_folders(
                    &bytes,
                    &workspace,
                    &accepted(&workspace),
                    4_096,
                    10
                )
                .expect("unmatched source is unadmitted")
                .is_empty()
            );
        }
    }

    #[test]
    fn test_metadata_refuses_malformed_foreign_root_and_count_or_byte_overflow() {
        let workspace = std::env::temp_dir().join("rift-cargo-metadata-workspace");
        let manifest = std::env::temp_dir().join("registry/helper/Cargo.toml");
        let bytes = metadata(&workspace, &serde_json::json!(SOURCE), &manifest);
        let accepted = accepted(&workspace);
        assert_eq!(
            CargoResolver::installed_folders(b"{", &workspace, &accepted, 4_096, 10),
            Err(CargoMetadataError::InvalidFormat)
        );
        assert_eq!(
            CargoResolver::installed_folders(
                &bytes,
                &workspace.join("other"),
                &accepted,
                4_096,
                10
            ),
            Err(CargoMetadataError::RootMismatch)
        );
        assert_eq!(
            CargoResolver::installed_folders(&bytes, &workspace, &accepted, 1, 10),
            Err(CargoMetadataError::Bound)
        );
        assert_eq!(
            CargoResolver::installed_folders(&bytes, &workspace, &accepted, 4_096, 0),
            Err(CargoMetadataError::Bound)
        );
        let relative = metadata(
            &workspace,
            &serde_json::json!(SOURCE),
            Path::new("helper/Cargo.toml"),
        );
        assert_eq!(
            CargoResolver::installed_folders(&relative, &workspace, &accepted, 4_096, 10),
            Err(CargoMetadataError::InvalidFormat)
        );
    }
    #[test]
    fn test_metadata_refuses_two_locked_owners_at_one_physical_root() {
        let workspace = std::env::temp_dir().join("rift-cargo-metadata-workspace");
        let mut inputs = RecordedInspector::default().with_file(workspace.join("Cargo.lock"), format!("version=4\n[[package]]\nname='helper'\nversion='1.0.0'\nsource='{SOURCE}'\n[[package]]\nname='helper'\nversion='2.0.0'\nsource='{SOURCE}'\n"));
        let context = crate::resolve_context(
            &workspace,
            &[ProjectPath("Cargo.toml".to_owned())],
            &[&CargoResolver::new()],
            &mut inputs,
            &[],
        );
        let root = std::env::temp_dir().join("registry/opaque-directory/helper/Cargo.toml");
        let bytes = serde_json::to_vec(
            &serde_json::json!({"version":1,"workspace_root":workspace,"packages":[
                {"name":"helper","version":"1.0.0","source":SOURCE,"manifest_path":root},
                {"name":"helper","version":"2.0.0","source":SOURCE,"manifest_path":root}
            ]}),
        )
        .expect("metadata fixture");
        assert_eq!(
            CargoResolver::installed_folders(&bytes, &workspace, &context, 4096, 10),
            Err(CargoMetadataError::ConflictingOwner)
        );
    }
    #[test]
    fn test_metadata_roots_keep_name_order_and_accepted_count_bound() {
        let workspace = std::env::temp_dir().join("rift-cargo-metadata-workspace");
        let mut inputs = RecordedInspector::default().with_file(workspace.join("Cargo.lock"), format!("version=4\n[[package]]\nname='helper'\nversion='1.0.0'\nsource='{SOURCE}'\n[[package]]\nname='beacon'\nversion='1.0.0'\nsource='{SOURCE}'\n"));
        let context = crate::resolve_context(
            &workspace,
            &[ProjectPath("Cargo.toml".to_owned())],
            &[&CargoResolver::new()],
            &mut inputs,
            &[],
        );
        let first = std::env::temp_dir().join("registry/a-beacon");
        let last = std::env::temp_dir().join("registry/z-helper");
        let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"workspace_root":workspace,"packages":[
            {"name":"helper","version":"1.0.0","source":SOURCE,"manifest_path":last.join("Cargo.toml")},
            {"name":"beacon","version":"1.0.0","source":SOURCE,"manifest_path":first.join("Cargo.toml")}
        ]})).expect("metadata fixture");
        let folders = CargoResolver::installed_folders(&bytes, &workspace, &context, 4096, 2)
            .expect("exact bounded roots");
        assert_eq!(
            folders
                .iter()
                .map(|folder| &folder.location)
                .collect::<Vec<_>>(),
            [&InstallLocation::Path(first), &InstallLocation::Path(last)]
        );
        assert_eq!(
            CargoResolver::installed_folders(&bytes, &workspace, &context, 4096, 1),
            Err(CargoMetadataError::Bound)
        );
    }

    #[test]
    fn test_metadata_refuses_one_locked_owner_at_different_manifest_roots() {
        let workspace = std::env::temp_dir().join("rift-cargo-metadata-workspace");
        let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"workspace_root":workspace,"packages":[
            {"name":"helper","version":"1.0.0","source":SOURCE,"manifest_path":std::env::temp_dir().join("registry/first/Cargo.toml")},
            {"name":"helper","version":"1.0.0","source":SOURCE,"manifest_path":std::env::temp_dir().join("registry/second/Cargo.toml")}
        ]})).expect("metadata fixture");
        assert_eq!(
            CargoResolver::installed_folders(&bytes, &workspace, &accepted(&workspace), 4096, 2),
            Err(CargoMetadataError::ConflictingOwner)
        );
    }
}
