//! The shared real-engine fixture: what every live suite supplies, and the
//! one place that turns it into a served workspace's `[engines.<name>]`
//! table.
//!
//! Each engine module beside this one (`rust_engine.rs`,
//! `typescript_engine.rs`, `toml_engine.rs`) supplies data - a program, its
//! arguments, the languages it claims, and whatever extra table content its
//! own environment or initialization options need - and nothing else; this
//! module owns the one conversion into the table's required lines. Adding
//! a language later means adding another fixture module that builds one of
//! these, not another test path.

/// One real engine's configuration data, independent of workspace or
/// timeouts: every live suite supplies exactly this much and no more.
///
/// Startup and request timeouts stay generous and fixed across every
/// fixture: a live engine's own cold-start cost, not the server's
/// absorption policy, is what the suites are proving.
pub(crate) struct EngineFixture {
    /// The table's own name, `[engines.<name>]`.
    pub(crate) name: &'static str,
    /// The executable name, resolved through `PATH`.
    pub(crate) program: &'static str,
    /// Arguments handed to the program.
    pub(crate) arguments: Vec<&'static str>,
    /// The languages this engine claims.
    pub(crate) languages: Vec<&'static str>,
    /// Extra TOML this engine's table needs beyond the required lines -
    /// an `[engines.<name>.environment]` table, an
    /// `[engines.<name>.initialization_options]` table, or both. Owned by
    /// the engine module, which alone knows its own shape; empty when
    /// there is nothing extra.
    pub(crate) extra_toml: String,
}

/// The startup and request timeout every fixture's table advertises.
const FIXTURE_TIMEOUT: &str = "2m";

impl EngineFixture {
    /// The `[engines.<name>]` table this fixture's data resolves to.
    pub(crate) fn configuration_toml(&self) -> String {
        let languages = self
            .languages
            .iter()
            .map(|language| format!("\"{language}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let arguments = self
            .arguments
            .iter()
            .map(|argument| format!("\"{argument}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[engines.{name}]\nprogram = \"{program}\"\narguments = [{arguments}]\n\
             languages = [{languages}]\nstartup_timeout = \"{timeout}\"\n\
             request_timeout = \"{timeout}\"\n{extra}",
            name = self.name,
            program = self.program,
            timeout = FIXTURE_TIMEOUT,
            extra = self.extra_toml,
        )
    }
}
