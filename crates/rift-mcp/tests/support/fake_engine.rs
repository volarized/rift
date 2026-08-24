//! Scaffolding for the suites driving rift-lsp's scripted `fake_engine`.
//!
//! Every scripted suite wires the binary into the workspace's own
//! `rift.toml` through an overlaid `PATH`, exactly as an operator's
//! `[engines.<name>]` table would resolve a real engine.

use std::path::PathBuf;

/// The directory holding the compiled `fake_engine` binary.
///
/// A test binary runs from `target/<profile>/deps`, and Cargo places
/// another crate's binary one level up. Running the suite with `rift-lsp`
/// in the invocation - the workspace suite does - builds the binary before
/// any test runs.
pub(crate) fn fake_engine_directory() -> PathBuf {
    let mut directory = std::env::current_exe().expect("the test binary has a path");
    directory.pop();
    if directory.ends_with("deps") {
        directory.pop();
    }
    assert!(
        directory.join("fake_engine").exists(),
        "fake_engine is missing from {}: build it first with `cargo test -p rift-lsp`",
        directory.display(),
    );
    directory
}

/// One `[engines.fake]` table resolving `fake_engine` through an overlaid
/// `PATH`, claiming `rust`.
pub(crate) fn engine_configuration(behavior: &str, request_timeout: &str) -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    let path_overlay = format!("{}:{inherited}", fake_engine_directory().display());
    format!(
        "[engines.fake]\nprogram = \"fake_engine\"\narguments = [\"{behavior}\"]\n\
         languages = [\"rust\"]\nrequest_timeout = \"{request_timeout}\"\n\n\
         [engines.fake.environment]\nPATH = \"{path_overlay}\"\n"
    )
}
