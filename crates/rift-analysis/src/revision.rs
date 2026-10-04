//! Analyzer revision from the checked source manifest, and the renderer that writes it.

mod manifest;

use std::sync::OnceLock;

use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_protocol::read::Digest;
use sha2::{Digest as _, Sha256};

pub use manifest::{ManifestError, analyzer_manifest_path, render_analyzer_manifest};

const ANALYZER_MANIFEST: &str = include_str!("analyzer-manifest.json");

/// Full content digest of the analyzer's checked source manifest.
#[must_use]
pub fn analyzer_digest() -> String {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST
        .get_or_init(|| format!("{:x}", Sha256::digest(ANALYZER_MANIFEST.as_bytes())))
        .clone()
}

/// Revision of the package analyzer and its shipped syntax inputs.
#[must_use]
pub fn analyzer_revision() -> Digest {
    static REVISION: OnceLock<Digest> = OnceLock::new();
    REVISION
        .get_or_init(|| {
            let rendered = analyzer_digest();
            Digest(rendered[..DIGEST_WIRE_CHARS].to_owned())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{ANALYZER_MANIFEST, analyzer_digest, analyzer_revision};
    use sha2::{Digest as _, Sha256};

    #[test]
    fn persisted_analyzer_digest_keeps_every_sha256_byte() {
        let expected = format!("{:x}", Sha256::digest(ANALYZER_MANIFEST.as_bytes()));
        let digest = analyzer_digest();
        assert_eq!(digest, expected);
        assert_eq!(digest.len(), 64);
        assert!(digest.starts_with(&analyzer_revision().0));
    }
}
