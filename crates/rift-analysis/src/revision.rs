//! Analyzer revision from the source manifest generated during compilation.

mod manifest;

use std::path::PathBuf;
use std::sync::OnceLock;

use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_protocol::read::Digest;

pub use manifest::{ManifestError, render_analyzer_manifest};

const ANALYZER_DIGEST: &str = env!("RIFT_ANALYZER_DIGEST");

/// The path the exporter writes the manifest to, relative to the repository root.
#[must_use]
pub fn analyzer_manifest_path() -> PathBuf {
    PathBuf::from("crates/rift-analysis/analyzer-manifest.json")
}

/// Full content digest of the analyzer's build-time source manifest.
#[must_use]
pub fn analyzer_digest() -> String {
    ANALYZER_DIGEST.to_owned()
}

/// Revision of the package analyzer and its shipped syntax inputs.
#[must_use]
pub fn analyzer_revision() -> Digest {
    static REVISION: OnceLock<Digest> = OnceLock::new();
    REVISION
        .get_or_init(|| Digest(ANALYZER_DIGEST[..DIGEST_WIRE_CHARS].to_owned()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{analyzer_digest, analyzer_revision};
    use sha2::{Digest as _, Sha256};

    #[test]
    fn persisted_analyzer_digest_keeps_every_sha256_byte() {
        let manifest = include_bytes!(concat!(env!("OUT_DIR"), "/analyzer-manifest.json"));
        let expected = data_encoding::HEXLOWER.encode(&Sha256::digest(manifest));
        let digest = analyzer_digest();
        assert_eq!(digest, expected);
        assert_eq!(digest.len(), 64);
        assert!(digest.starts_with(&analyzer_revision().0));
    }
}
