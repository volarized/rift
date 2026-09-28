//! Analyzer revision from the checked source manifest, and the renderer that writes it.

mod manifest;

use std::sync::OnceLock;

use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_protocol::read::Digest;
use sha2::{Digest as _, Sha256};

pub use manifest::{ManifestError, analyzer_manifest_path, render_analyzer_manifest};

const ANALYZER_MANIFEST: &str = include_str!("analyzer-manifest.json");

/// Revision of the package analyzer and its shipped syntax inputs.
#[must_use]
pub fn analyzer_revision() -> Digest {
    static REVISION: OnceLock<Digest> = OnceLock::new();
    REVISION
        .get_or_init(|| {
            let rendered = format!("{:x}", Sha256::digest(ANALYZER_MANIFEST.as_bytes()));
            Digest(rendered[..DIGEST_WIRE_CHARS].to_owned())
        })
        .clone()
}
