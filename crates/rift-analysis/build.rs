//! Records the analyzer manifest and its digest from the sources this build uses.

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

#[path = "src/revision/manifest.rs"]
#[allow(
    unreachable_pub,
    reason = "the library re-exports the renderer and its error"
)]
mod manifest;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("the analyzer crate has no workspace root")?;
    println!("cargo::rerun-if-changed=build.rs");
    for path in std::iter::once(manifest::LOCKFILE_PATH).chain(manifest::ANALYZED_SOURCES) {
        println!("cargo::rerun-if-changed={}", root.join(path).display());
    }

    let document = manifest::render_analyzer_manifest(root)?;
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(document.as_bytes()));
    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("Cargo did not set OUT_DIR")?);
    std::fs::write(output.join("analyzer-manifest.json"), document)?;
    println!("cargo::rustc-env=RIFT_ANALYZER_DIGEST={digest}");
    Ok(())
}
