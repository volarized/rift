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
    /// Where the process definition is written: a shared top-level table, or
    /// the one language entry that uses it.
    pub(crate) placement: LspPlacement,
    /// Shared top-level LSP process name, read under
    /// [`LspPlacement::Named`] alone: an inline process is keyed by the
    /// language entry that holds it.
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

/// Which of the two accepted spellings a fixture writes its process in.
///
/// Cargo compiles each live suite as its own binary and each binary carries
/// exactly one engine module, so the spelling that suite does not use is
/// unconstructed there.
#[derive(Clone, Copy)]
#[expect(
    dead_code,
    reason = "each live suite constructs one placement; the other is unused in that binary"
)]
pub(crate) enum LspPlacement {
    /// One `[lsp.<name>]` table every listed identity selects by name.
    Named,
    /// One `[languages.<identity>.lsp]` table belonging to that entry alone.
    /// Only a fixture serving exactly one identity can use it.
    Inline,
}

impl EngineFixture {
    /// Language entries and the LSP process this fixture resolves to.
    pub(crate) fn configuration_toml(&self) -> String {
        let command = std::iter::once(self.program)
            .chain(self.arguments.iter().copied())
            .map(|argument| format!("\"{argument}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let bounds = format!(
            "command = [{command}]\nstartup_timeout = \"{timeout}\"\n\
             request_timeout = \"{timeout}\"\n{extra}",
            timeout = FIXTURE_TIMEOUT,
            extra = self.extra_toml,
        );
        match self.placement {
            LspPlacement::Inline => {
                let [identity] = self.languages.as_slice() else {
                    panic!("an inline process belongs to exactly one language entry");
                };
                format!("[languages.{}.lsp]\n{bounds}", Self::table_key(identity))
            }
            LspPlacement::Named => {
                let mut languages = String::new();
                for language in &self.languages {
                    write!(
                        languages,
                        "[languages.{}]\nlsp = \"{}\"\n",
                        Self::table_key(language),
                        self.name
                    )
                    .expect("writing to a String cannot fail");
                }
                format!("{languages}[lsp.{name}]\n{bounds}", name = self.name)
            }
        }
    }

    /// One exact identity spelled as a TOML table key: a dialect carries `:`,
    /// which only a quoted key accepts.
    fn table_key(identity: &str) -> String {
        if identity.contains(':') {
            format!("\"{identity}\"")
        } else {
            identity.to_owned()
        }
    }
}
