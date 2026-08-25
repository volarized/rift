//! The `[search.semantic]` table every fixture outside the live suite declares.
//!
//! Rift ships the semantic tier on: a workspace with no `rift.toml` acquires the
//! default model from the hub, and `live_semantic_search` is the suite that proves
//! it. Every other fixture opts out the same way an operator would, for two
//! reasons. A hermetic suite must not write into the developer's own Hugging Face
//! cache. And on a runner with no network a default-on tier would spend its whole
//! retry budget inside a detached task nobody waits on, so the suite would pay for
//! an acquisition no test reads.
//!
//! `rift-mcp`'s own unit tests declare the same table again, in `server.rs`: an
//! integration test and a unit test are two crates, and a value shared between
//! them would have to leave the library's public surface to do it.

/// The table that turns the semantic tier off for one fixture workspace.
pub(crate) const SEMANTIC_DISABLED: &str = "[search.semantic]\ndisabled = true\n";
