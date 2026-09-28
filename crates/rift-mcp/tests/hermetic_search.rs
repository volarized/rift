//! The `[search.vector]` and `[dependencies]` tables every fixture outside the live suite
//! declares.
//!
//! Rift ships the vector ranking on: a workspace with no `rift.toml` acquires the
//! default model from the hub, and `live_vector_search` is the suite that proves
//! it. Every other fixture opts out the same way an operator would, for two
//! reasons. A hermetic suite must not write into the developer's own Hugging Face
//! cache. And on a runner with no network a default-on tier would spend its whole
//! retry budget inside a detached task nobody waits on, so the suite would pay for
//! an acquisition no test reads.
//!
//! Rift also runs the standard library version probes by default, and a probe answers
//! from the machine's own toolchain, so a fixture's package context would follow
//! whatever `rustc` and `node` the runner installed. `resolution = "static"` reads the
//! pins alone and runs no program.
//!
//! `rift-mcp`'s own unit tests declare the same tables again, in `server.rs`: an
//! integration test and a unit test are two crates, and a value shared between
//! them would have to leave the library's public surface to do it.

/// The tables that turn the vector ranking and the version probes off for one fixture
/// workspace.
pub(crate) const HERMETIC_TABLES: &str =
    "[search.vector]\ndisabled = true\n\n[dependencies]\nresolution = \"static\"\n";
