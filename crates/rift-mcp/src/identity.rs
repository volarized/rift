//! Exact identity shared by MCP initialize data, server locks, and stored rows.
//!
//! The `rift` binary's build script records the git commit it was built from, and whether
//! the working tree held uncommitted changes; `main` hands both here as a [`BuildCheckout`],
//! so a process names its build without reading its executable. The version is the package
//! version with that commit as semantic-versioning build metadata. A build from a tree with
//! uncommitted changes also names the executable's size and modification time, read from file
//! metadata, so a rebuilt development binary never answers with the identity of the one
//! before it.

use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use rift_protocol::lock::ProductIdentity;
use rmcp::model::MetaObject;
use serde_json::json;
use sha2::{Digest as _, Sha256};

pub(crate) const RIFT_IDENTITY_META_KEY: &str = "sh.volar/rift";

/// The package version every build names first.
const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The build metadata identifier that marks a build from a working tree with uncommitted
/// changes.
const DIRTY_MARK: &str = "dirty";

/// The checkout one executable was built from, as the `rift` binary's build script recorded
/// it.
///
/// A server or proxy started through a library entry point rather than the binary - a test
/// building a server in its own process - serves as [`BuildCheckout::Unversioned`]: no build
/// script ran for it, so the package version is all it can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildCheckout {
    /// No git: a source archive, no `git` on the build machine, or a library entry point.
    Unversioned,
    /// A commit with nothing uncommitted, which names the sources exactly.
    Clean(&'static str),
    /// A commit the working tree had uncommitted changes against.
    Dirty(&'static str),
}

impl BuildCheckout {
    /// The checkout the build script's `RIFT_BUILD_COMMIT` and `RIFT_BUILD_DIRTY` values
    /// describe: an empty commit names no checkout, and a non-empty dirty value marks
    /// uncommitted changes.
    #[must_use]
    pub const fn recorded(commit: &'static str, dirty: &'static str) -> Self {
        match (commit.is_empty(), dirty.is_empty()) {
            (true, _) => Self::Unversioned,
            (false, true) => Self::Clean(commit),
            (false, false) => Self::Dirty(commit),
        }
    }

    /// The version `executable`, built from this checkout, names; only a dirty checkout
    /// reads anything from `executable`, and then only its metadata.
    ///
    /// # Errors
    ///
    /// Returns the metadata read's failure for a dirty checkout.
    pub fn product_version(self, executable: &Path) -> io::Result<String> {
        self.version(|| ExecutableStamp::read(executable))
    }

    /// The version, reading the executable's metadata through `stamp` only for a dirty
    /// checkout.
    fn version(self, stamp: impl FnOnce() -> io::Result<ExecutableStamp>) -> io::Result<String> {
        match self {
            Self::Unversioned => Ok(PACKAGE_VERSION.to_owned()),
            Self::Clean(commit) => Ok(format!("{PACKAGE_VERSION}+{commit}")),
            Self::Dirty(commit) => {
                let stamp = stamp()?;
                Ok(format!(
                    "{PACKAGE_VERSION}+{commit}.{DIRTY_MARK}.{size}.{modified}",
                    size = stamp.size_bytes,
                    modified = stamp.modified_ns,
                ))
            }
        }
    }

    /// The version without the executable's metadata: what `rift --version` prints for a
    /// dirty build whose executable cannot be read. A lock never carries it; the product
    /// identity refuses the case instead.
    #[must_use]
    pub fn version_without_stamp(self) -> String {
        match self {
            Self::Unversioned => PACKAGE_VERSION.to_owned(),
            Self::Clean(commit) => format!("{PACKAGE_VERSION}+{commit}"),
            Self::Dirty(commit) => format!("{PACKAGE_VERSION}+{commit}.{DIRTY_MARK}"),
        }
    }
}

/// The file metadata that tells one build of a dirty checkout from the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExecutableStamp {
    /// The executable's length.
    size_bytes: u64,
    /// The executable's modification time, in nanoseconds since the Unix epoch.
    modified_ns: u128,
}

impl ExecutableStamp {
    /// Reads `executable`'s length and modification time, and none of its bytes.
    ///
    /// # Errors
    ///
    /// Returns the metadata read's failure, or an error for a modification time before
    /// the Unix epoch.
    fn read(executable: &Path) -> io::Result<Self> {
        let metadata = std::fs::metadata(executable)?;
        let modified_ns = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                io::Error::other(format!(
                    "executable modification time precedes the Unix epoch: {error}"
                ))
            })?
            .as_nanos();
        Ok(Self {
            size_bytes: metadata.len(),
            modified_ns,
        })
    }
}

/// This process's product version and canonical tool identity, for an executable built from
/// `checkout`.
///
/// Only a dirty checkout reads the running executable, and only its metadata; the work runs
/// on the blocking pool.
pub(crate) async fn product_identity(checkout: BuildCheckout) -> io::Result<ProductIdentity> {
    tokio::task::spawn_blocking(move || product_identity_of(checkout, &std::env::current_exe()?))
        .await
        .map_err(|error| io::Error::other(format!("product identity task failed: {error}")))?
}

/// The identity of `executable`, built from `checkout`: the `rift` binary itself, or the one
/// beside a test that spawns it.
///
/// # Errors
///
/// Returns the metadata read's failure for a dirty checkout.
pub fn product_identity_of(
    checkout: BuildCheckout,
    executable: &Path,
) -> io::Result<ProductIdentity> {
    Ok(ProductIdentity {
        version: checkout.product_version(executable)?,
        schema_digest: schema_digest().to_owned(),
    })
}

/// The SHA-256 of the canonical served tool document, which the compiled tool router alone
/// decides, so one process computes it once.
fn schema_digest() -> &'static str {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST.get_or_init(|| {
        format!(
            "{:x}",
            Sha256::digest(crate::schema::schema_document().as_bytes())
        )
    })
}

/// Builds initialize metadata carrying one product identity.
pub(crate) fn identity_meta(identity: &ProductIdentity) -> MetaObject {
    let mut meta = MetaObject::new();
    meta.insert(RIFT_IDENTITY_META_KEY.to_owned(), json!(identity));
    meta
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use rift_protocol::lock::{SERVER_TOKEN_LENGTH, ServerLock};

    use super::{BuildCheckout, ExecutableStamp, PACKAGE_VERSION};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const COMMIT: &str = "b006b8433ba06679f06a3c7f0743d65634d32c34";

    fn unread() -> std::io::Result<ExecutableStamp> {
        Err(std::io::Error::other("a clean build reads no executable"))
    }

    #[test]
    fn recorded_values_name_their_checkout() {
        assert_eq!(BuildCheckout::recorded("", ""), BuildCheckout::Unversioned);
        assert_eq!(
            BuildCheckout::recorded("", "true"),
            BuildCheckout::Unversioned
        );
        assert_eq!(
            BuildCheckout::recorded(COMMIT, ""),
            BuildCheckout::Clean(COMMIT)
        );
        assert_eq!(
            BuildCheckout::recorded(COMMIT, "true"),
            BuildCheckout::Dirty(COMMIT)
        );
    }

    #[test]
    fn a_build_with_no_git_names_the_package_version_alone() -> TestResult {
        assert_eq!(BuildCheckout::Unversioned.version(unread)?, PACKAGE_VERSION);
        Ok(())
    }

    #[test]
    fn a_clean_build_names_its_commit_and_reads_no_executable() -> TestResult {
        assert_eq!(
            BuildCheckout::Clean(COMMIT).product_version(Path::new("/nonexistent/rift"))?,
            format!("{PACKAGE_VERSION}+{COMMIT}")
        );
        Ok(())
    }

    #[test]
    fn a_dirty_build_names_the_executables_size_and_modification_time() -> TestResult {
        let stamp = ExecutableStamp {
            size_bytes: 78_008_464,
            modified_ns: 1_790_239_195_123_456_789,
        };
        assert_eq!(
            BuildCheckout::Dirty(COMMIT).version(|| Ok(stamp))?,
            format!("{PACKAGE_VERSION}+{COMMIT}.dirty.78008464.1790239195123456789")
        );
        let unreadable = BuildCheckout::Dirty(COMMIT).product_version(Path::new("/nonexistent"));
        assert!(
            unreadable.is_err(),
            "a dirty build never names itself without its stamp"
        );
        Ok(())
    }

    /// Every version this module mints passes the lock contract, so a server can publish
    /// it and a proxy can read it back.
    #[test]
    fn every_minted_version_passes_the_lock_contract() -> TestResult {
        let stamp = ExecutableStamp {
            size_bytes: 1,
            modified_ns: 1,
        };
        for checkout in [
            BuildCheckout::Unversioned,
            BuildCheckout::Clean(COMMIT),
            BuildCheckout::Dirty(COMMIT),
        ] {
            let lock = ServerLock {
                port: 12_345,
                token: "a".repeat(SERVER_TOKEN_LENGTH),
                pid: 4_242,
                identity: rift_protocol::lock::ProductIdentity {
                    version: checkout.version(|| Ok(stamp))?,
                    schema_digest: "b".repeat(64),
                },
            };
            assert_eq!(lock.validate(), Ok(()), "{checkout:?}");
        }
        Ok(())
    }

    /// A development binary rebuilt from the same dirty tree is another file: the next
    /// link writes it again, so its modification time, and usually its size, move. Its
    /// identity therefore never equals the one a server started from the earlier build
    /// published.
    #[test]
    fn a_rebuilt_dirty_binary_never_names_the_earlier_builds_identity() -> TestResult {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("rift");
        fs::write(&executable, b"first build")?;
        let first_written = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        fs::File::options()
            .write(true)
            .open(&executable)?
            .set_modified(first_written)?;
        let checkout = BuildCheckout::Dirty(COMMIT);
        let earlier = checkout.product_version(&executable)?;

        fs::write(&executable, b"first build")?;
        let relinked = first_written + Duration::from_nanos(1);
        fs::File::options()
            .write(true)
            .open(&executable)?
            .set_modified(relinked)?;
        let same_bytes_relinked = checkout.product_version(&executable)?;
        assert_ne!(
            same_bytes_relinked, earlier,
            "a relink with identical bytes still moves the modification time"
        );

        fs::write(&executable, b"second build, grown")?;
        fs::File::options()
            .write(true)
            .open(&executable)?
            .set_modified(first_written)?;
        let rebuilt = checkout.product_version(&executable)?;
        assert_ne!(
            rebuilt, earlier,
            "a rebuild with the earlier modification time differs in size"
        );
        Ok(())
    }

    #[test]
    fn a_stamp_before_the_unix_epoch_is_refused() -> TestResult {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("rift");
        fs::write(&executable, b"rift")?;
        let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
        let applied = fs::File::options()
            .write(true)
            .open(&executable)?
            .set_modified(before_epoch);
        // Some filesystems cannot store a time before the epoch; there is nothing to read.
        if applied.is_ok() && fs::metadata(&executable)?.modified()? < SystemTime::UNIX_EPOCH {
            assert!(ExecutableStamp::read(&executable).is_err());
        }
        Ok(())
    }

    #[test]
    fn the_version_without_a_stamp_keeps_the_dirty_mark() {
        assert_eq!(
            BuildCheckout::Dirty(COMMIT).version_without_stamp(),
            format!("{PACKAGE_VERSION}+{COMMIT}.dirty")
        );
        assert_eq!(
            BuildCheckout::Clean(COMMIT).version_without_stamp(),
            format!("{PACKAGE_VERSION}+{COMMIT}")
        );
        assert_eq!(
            BuildCheckout::Unversioned.version_without_stamp(),
            PACKAGE_VERSION
        );
    }

    /// The async identity reads the running executable, and equals what the synchronous
    /// form computes for the same executable.
    #[tokio::test]
    async fn product_identity_names_the_running_executable() -> TestResult {
        let executable = std::env::current_exe()?;
        for checkout in [BuildCheckout::Unversioned, BuildCheckout::Dirty(COMMIT)] {
            let identity = super::product_identity(checkout).await?;
            assert_eq!(identity, super::product_identity_of(checkout, &executable)?);
        }
        assert_eq!(super::schema_digest(), super::schema_digest());
        Ok(())
    }
}
