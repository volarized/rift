//! Shared real-engine fixture data and its served workspace LSP configuration.
//!
//! Each engine module beside this one (`rust_engine.rs`,
//! `typescript_engine.rs`, `toml_engine.rs`) supplies data - a program, its
//! arguments, the language identities it serves, and whatever extra table content its
//! own environment or initialization options need - and nothing else; this
//! module owns the one conversion into the required lines. Adding
//! a language later means adding another fixture module that builds one of
//! these, not another test path.

use std::fmt::Write as _;

/// One real engine's configuration data, independent of workspace or
/// timeouts: every live suite supplies exactly this much and no more.
///
/// Startup and request timeouts stay generous and fixed across every
/// fixture: a live engine's own cold-start cost, not the server's
/// absorption policy, is what the suites are proving.
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
    /// an `[lsp.<name>.environment]` table, an
    /// `[lsp.<name>.initialization_options]` table, or both. Owned by
    /// the engine module, which alone knows its own shape; empty when
    /// there is nothing extra.
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
