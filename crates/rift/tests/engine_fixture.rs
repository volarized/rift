//! Shared real-engine fixture data and its served workspace LSP configuration.
//!
//! `rust_engine.rs` beside this module supplies data - a program, its
//! arguments, the language identities it serves, and whatever extra table content
//! its own environment needs - and nothing else; this module owns the one
//! conversion into the required lines. Adding a language later
//! means adding another fixture module that builds one of these, not
//! another test path. This mirrors `rift-mcp`'s own `engine_fixture.rs`;
//! Cargo compiles each crate's integration tests as their own binary, so
//! the module is duplicated rather than shared.

use std::fmt::Write as _;

/// One real engine's configuration data, independent of workspace or
/// timeouts: every end-to-end case supplies exactly this much and no
/// more.
pub(crate) struct EngineFixture {
    /// Shared top-level LSP process name.
    pub(crate) name: &'static str,
    /// The executable name, resolved through `PATH`.
    pub(crate) program: &'static str,
    /// Arguments handed to the program.
    pub(crate) arguments: Vec<&'static str>,
    /// Exact language identities this engine serves.
    pub(crate) languages: Vec<&'static str>,
    /// Extra TOML this LSP process needs beyond the required lines -
    /// an `[lsp.<name>.environment]` table, most often. Owned by the
    /// engine module, which alone knows its own shape; empty when there
    /// is nothing extra.
    pub(crate) extra_toml: String,
}

/// Startup and request timeout every fixture advertises.
const FIXTURE_TIMEOUT: &str = "2m";

impl EngineFixture {
    /// Language bindings and named LSP process this fixture resolves to.
    pub(crate) fn configuration_toml(&self) -> String {
        let mut languages = String::new();
        for language in &self.languages {
            let table = if language.contains(':') {
                format!("\"{language}\"")
            } else {
                (*language).to_owned()
            };
            write!(languages, "[languages.{table}]\nlsp = \"{}\"\n", self.name)
                .expect("writing to a String cannot fail");
        }
        let command = std::iter::once(self.program)
            .chain(self.arguments.iter().copied())
            .map(|argument| format!("\"{argument}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{languages}[lsp.{name}]\ncommand = [{command}]\nstartup_timeout = \"{timeout}\"\n\
             request_timeout = \"{timeout}\"\n{extra}",
            name = self.name,
            timeout = FIXTURE_TIMEOUT,
            extra = self.extra_toml,
        )
    }
}
