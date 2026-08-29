//! Exact identity shared by MCP initialize data and server locks.

use std::fs;
use std::io::{self, Read as _};
use std::path::Path;

use rift_core::constants::RELEASE_BINARY_BYTES_MAX;
use rift_protocol::lock::ProductIdentity;
use rmcp::model::MetaObject;
use serde_json::json;
use sha2::{Digest as _, Sha256};

pub(crate) const RIFT_IDENTITY_META_KEY: &str = "sh.volar/rift";

/// Maximum bytes hashed from current executable.
///
/// Release builds keep release binary bound. Debug and coverage builds may
/// embed debug data in the executable, so they keep a separate explicit bound.
const DEBUG_EXECUTABLE_BYTES_MAX: u64 = RELEASE_BINARY_BYTES_MAX * 16;
const CURRENT_EXECUTABLE_BYTES_MAX: u64 = if cfg!(debug_assertions) {
    DEBUG_EXECUTABLE_BYTES_MAX
} else {
    RELEASE_BINARY_BYTES_MAX
};

/// Computes this process's package, executable, and canonical tool identity.
pub(crate) async fn product_identity() -> io::Result<ProductIdentity> {
    tokio::task::spawn_blocking(|| product_identity_for(&std::env::current_exe()?))
        .await
        .map_err(|error| io::Error::other(format!("product identity task failed: {error}")))?
}

fn product_identity_for(executable: &Path) -> io::Result<ProductIdentity> {
    Ok(ProductIdentity {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        executable_digest: executable_digest(executable)?,
        schema_digest: format!(
            "{:x}",
            Sha256::digest(crate::schema::schema_document().as_bytes())
        ),
    })
}

fn executable_digest(executable: &Path) -> io::Result<String> {
    let file = fs::File::open(executable)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("current executable is not a regular file"));
    }
    if metadata.len() == 0 || metadata.len() > CURRENT_EXECUTABLE_BYTES_MAX {
        return Err(io::Error::other(format!(
            "current executable size {} is outside 1..={CURRENT_EXECUTABLE_BYTES_MAX} bytes",
            metadata.len()
        )));
    }
    let mut bounded = file.take(CURRENT_EXECUTABLE_BYTES_MAX + 1);
    let mut digest = Sha256::new();
    let copied = io::copy(&mut bounded, &mut digest)?;
    if copied > CURRENT_EXECUTABLE_BYTES_MAX {
        return Err(io::Error::other(format!(
            "current executable exceeds {CURRENT_EXECUTABLE_BYTES_MAX} bytes"
        )));
    }
    Ok(format!("{:x}", digest.finalize()))
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

    use sha2::{Digest as _, Sha256};

    #[test]
    fn executable_digest_hashes_exact_file_bytes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let executable = directory.path().join("rift");
        fs::write(&executable, b"rift executable").expect("fixture");
        assert_eq!(
            super::executable_digest(&executable).expect("digest"),
            format!("{:x}", Sha256::digest(b"rift executable"))
        );
    }

    #[test]
    fn executable_digest_rejects_non_file_empty_and_oversized_inputs() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let non_file =
            super::executable_digest(directory.path()).expect_err("directory must not hash");
        assert!(non_file.to_string().contains("not a regular file"));

        let empty = directory.path().join("empty");
        fs::write(&empty, []).expect("empty fixture");
        let empty_error = super::executable_digest(&empty).expect_err("empty file must not hash");
        assert!(empty_error.to_string().contains("outside"));

        let oversized = directory.path().join("oversized");
        let file = fs::File::create(&oversized).expect("oversized fixture");
        file.set_len(super::CURRENT_EXECUTABLE_BYTES_MAX + 1)
            .expect("sparse oversized fixture");
        let oversized_error =
            super::executable_digest(&oversized).expect_err("oversized file must not hash");
        assert!(oversized_error.to_string().contains("outside"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_executable_bound_covers_instrumented_builds() {
        assert_eq!(
            super::CURRENT_EXECUTABLE_BYTES_MAX,
            rift_core::constants::RELEASE_BINARY_BYTES_MAX * 16
        );
    }
}
