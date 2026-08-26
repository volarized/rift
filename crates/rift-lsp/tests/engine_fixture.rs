//! The shared real-engine fixture: what every live suite supplies, and the
//! one place that turns it into a launch.
//!
//! Each engine module beside this one (`rust_engine.rs`,
//! `typescript_engine.rs`) supplies data - a program, its arguments, the
//! environment overlay it needs, and its initialization options - and
//! nothing else; this module owns the one conversion into
//! [`rift_lsp::session::EngineLaunch`]. Adding a language later means
//! adding another fixture module that builds one of these, not another
//! test path.

use std::collections::BTreeMap;
use std::time::Duration;

use rift_lsp::session::EngineLaunch;
use serde_json::Value;

/// One real engine's launch data, independent of workspace or timeouts:
/// every live suite supplies exactly this much and no more.
///
/// Startup and request timeouts stay generous and fixed across every
/// fixture: a live engine's own cold-start cost, not this session's
/// policy, is what the suites are proving.
pub(crate) struct EngineFixture {
    /// The executable name, resolved through `PATH`.
    pub(crate) program: &'static str,
    /// Arguments handed to the program.
    pub(crate) arguments: Vec<String>,
    /// Environment entries laid over the inherited environment.
    pub(crate) environment: BTreeMap<String, String>,
    /// Options handed to the engine in the initialize request, verbatim.
    pub(crate) initialization_options: Option<Value>,
}

/// Wall-clock bound on the handshake and on each later request: an engine
/// answers initialize before it has finished loading a project and keeps
/// working afterward, so the bound is generous - and still a bound.
const FIXTURE_TIMEOUT: Duration = Duration::from_mins(2);

/// Bytes of standard error the fixture's launch keeps captured.
const FIXTURE_STDERR_CAPTURE_BYTES: usize = 65_536;

impl EngineFixture {
    /// The launch this fixture's data resolves to, under the shared
    /// timeout and capture bounds every live suite runs with.
    pub(crate) fn launch(&self) -> EngineLaunch {
        EngineLaunch {
            program: self.program.to_owned(),
            arguments: self.arguments.clone(),
            environment: self.environment.clone(),
            initialization_options: self.initialization_options.clone(),
            startup_timeout: FIXTURE_TIMEOUT,
            request_timeout: FIXTURE_TIMEOUT,
            stderr_capture_bytes: FIXTURE_STDERR_CAPTURE_BYTES,
        }
    }
}
