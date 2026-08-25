//! The gate the live semantic-search suite stands behind.
//!
//! `RIFT_SEARCH_LIVE` gates the one suite that reaches the model hub, the way
//! `RIFT_ENGINE_LIVE` gates the ones that reach a language server. Rust has no
//! built-in skip: an unset gate prints one visible line and the test returns
//! early. When the variable is set the tests run and fail if the hub is
//! unavailable - CI must never green-skip what it meant to run.
//!
//! Every other fixture in this crate turns the semantic tier off;
//! `hermetic_search.rs` carries that table and the reason for it.

/// The environment variable that turns the live semantic-search tests on.
pub(crate) const SEARCH_LIVE_VARIABLE: &str = "RIFT_SEARCH_LIVE";

/// Whether the live semantic-search tests run; an unset gate prints the skip line.
pub(crate) fn search_live() -> bool {
    if std::env::var_os(SEARCH_LIVE_VARIABLE).is_some() {
        return true;
    }
    eprintln!("skipped: {SEARCH_LIVE_VARIABLE} unset");
    false
}
