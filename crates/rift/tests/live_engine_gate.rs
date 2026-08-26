//! The gate every live-engine suite stands behind.
//!
//! `RIFT_ENGINE_LIVE` gates every live suite, here and in rift-lsp and
//! rift-mcp, with one variable and one message spelling. Rust has no
//! built-in skip: an unset gate prints one visible line and the test
//! returns early. When the variable is set the tests run and fail if the
//! engine is unavailable - CI must never green-skip what it meant to run.

/// The environment variable that turns the live-engine tests on.
pub(crate) const ENGINE_LIVE_VARIABLE: &str = "RIFT_ENGINE_LIVE";

/// Whether the live-engine tests run; an unset gate prints the skip line.
pub(crate) fn engine_live() -> bool {
    if std::env::var_os(ENGINE_LIVE_VARIABLE).is_some() {
        return true;
    }
    eprintln!("skipped: {ENGINE_LIVE_VARIABLE} unset");
    false
}
