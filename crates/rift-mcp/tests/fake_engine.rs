//! Scaffolding for the suites driving rift-lsp's scripted `fake_engine`.
//!
//! Every scripted suite wires the binary into the workspace's own
//! `rift.toml` through an overlaid `PATH`, exactly as an operator's
//! `[engines.<name>]` table would resolve a real engine.

use std::path::{Path, PathBuf};

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

/// The same engine table with a lifecycle log and a narrow retry budget.
///
/// The log is how a test counts the engine's requests: the behaviors that
/// act once and then serve read their own count back from it, and the
/// assertions read the same lines. The waits are held at a millisecond so
/// the suite spends no time on them; the shape of the growing wait is
/// proven by the policy's own unit tests.
pub(crate) fn counted(configuration: &str, log: &Path, attempts: u64) -> String {
    format!(
        "{configuration}RIFT_FAKE_ENGINE_LIFECYCLE_LOG = \"{}\"\n\n\
         [engines.fake.retry]\nattempts = {attempts}\ndelay = \"1ms\"\n\
         delay_limit = \"1ms\"\n",
        log.display()
    )
}

/// Lines of one lifecycle event the engine recorded.
pub(crate) fn recorded(log: &Path, event: &str) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == event)
        .count()
}
