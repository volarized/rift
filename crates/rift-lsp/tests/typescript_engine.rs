//! The typescript-language-server launch data.
//!
//! The engine runs from the fixture-local executable `typescript_install.rs`
//! installs from the committed lockfile.

use std::collections::BTreeMap;

use serde_json::json;

use crate::engine_fixture::EngineFixture;
use crate::typescript_install::LANGUAGE_SERVER_PROGRAM;

/// The fixture data the shared harness turns into one launch.
pub(crate) fn fixture() -> EngineFixture {
    EngineFixture {
        program: LANGUAGE_SERVER_PROGRAM,
        arguments: vec!["--stdio".to_owned()],
        environment: BTreeMap::new(),
        initialization_options: Some(json!({
            "tsserver": { "useSyntaxServer": "never" }
        })),
    }
}
