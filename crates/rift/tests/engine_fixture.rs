//! The shared real-engine fixture: what an end-to-end case supplies, and
//! the one place that turns it into a served workspace's
//! `[engines.<name>]` table.
//!
//! `rust_engine.rs` beside this module supplies data - a program, its
//! arguments, the languages it claims, and whatever extra table content
//! its own environment needs - and nothing else; this module owns the one
//! conversion into the table's required lines. Adding a language later
//! means adding another fixture module that builds one of these, not
//! another test path. This mirrors `rift-mcp`'s own `engine_fixture.rs`;
//! Cargo compiles each crate's integration tests as their own binary, so
//! the module is duplicated rather than shared.

/// One real engine's configuration data, independent of workspace or
/// timeouts: every end-to-end case supplies exactly this much and no
/// more.
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
    /// an `[engines.<name>.environment]` table, most often. Owned by the
    /// engine module, which alone knows its own shape; empty when there
    /// is nothing extra.
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
