//! Text of tool answers and execution errors, written through [`TextWriter`].
//!
//! Each answer type implements [`Render`]. [`text_of`] renders one answer under the output byte
//! limit. Every text is laid out for reading, in the manner of a compact command: the normal state
//! is implicit, and only exceptional metadata is written. An answer is a sequence of counted
//! sections, each a title at column 0 and its entries indented by one level. A level is one
//! indent unit of the writer, a tab; the example shows each level as two spaces. `search` and
//! `get_symbol` answers:
//!
//! ```text
//! 2 results · page 1/3
//!   [1] pub fn load_config(path: &Path) -> Result<Config, ConfigError>
//!       src/config.rs:10 · name
//!       rift://symbol/rust/src/config.rs/load_config
//!       Loads the workspace configuration from `rift.toml`.
//!
//!       pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
//!           parse_config(&text)
//!       }
//!   [2] src/lib.rs:7 · content
//!           let config = load_config(&arguments.path)?;
//! ```
//!
//! The title counts the results and names the page when there are several; the page is the only
//! pagination fact. One item has no marker and indents its lines by one level. Several items each
//! start with `[n] `
//! at one level and indent their further lines by two. Text an answer copies, such as source,
//! keeps its own whitespace after the indent. No blank line separates two items or two sections.
//! The hits a relationship walk reached are written as a tree; see the `walk` module.
//! `nodes` answers, failures, and workspace pages follow the same principles; see the
//! `nodes`, `error`, and `workspace` modules.

mod error;
mod facts;
mod layout;
mod nodes;
mod query;
mod search;
mod symbol;
mod walk;
mod workspace;

use rift_protocol::error::ErrorData;
use rift_protocol::read::{GetSymbolResult, NodesResult};
use rift_protocol::search::SearchResult;
use rift_protocol::workspace::WorkspaceResourcePage;

use super::text::{OUTPUT_TEXT_BYTES_MAX, TextError, TextWriter};
use super::warnings::{record_documentation, record_warnings};
use layout::Page;

pub(crate) use error::RegisteredFailure;

/// An answer that writes itself as text.
pub(crate) trait Render {
    /// Whether this typed answer reports a failed tool call.
    fn is_error(&self) -> bool {
        false
    }

    /// Records read warnings and clears them before serialization to a client.
    fn log_warnings(&mut self) {}

    /// Writes this answer into `out`.
    ///
    /// # Errors
    ///
    /// Returns the first failure of `out`: overflow, an unsupported shape, or a `Serialize`
    /// failure.
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError>;
}

/// Renders `answer` as text under the output byte limit.
///
/// # Errors
///
/// Returns the first failure of the writer. Overflow of the byte limit yields no text.
pub(crate) fn text_of<T: Render + ?Sized>(answer: &T) -> Result<String, TextError> {
    let mut out = TextWriter::new(OUTPUT_TEXT_BYTES_MAX);
    answer.render(&mut out)?;
    out.finish()
}

impl Render for SearchResult {
    fn log_warnings(&mut self) {
        record_warnings(&mut self.warnings);
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let Self {
            view,
            revision,
            results,
            pagination,
            ..
        } = self;
        let page = Page { pagination };
        search::answer(out, &page, results)?;
        layout::revision(out, revision.as_ref())?;
        if let Some(view) = view {
            out.raw_line(
                0,
                &format!("view {} · expires {}", view.id.as_str(), view.expires_at),
            )?;
        }
        Ok(())
    }
}

impl Render for rift_protocol::search_read::SearchReadResult {
    fn log_warnings(&mut self) {
        match self {
            Self::Results(result) => result.log_warnings(),
            Self::Unavailable(result) => result.log_warnings(),
        }
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        match self {
            Self::Results(result) => result.render(out),
            Self::Unavailable(result) => result.render(out),
        }
    }
}

impl Render for rift_protocol::search_read::SearchUnavailable {
    fn log_warnings(&mut self) {
        let Self::Unavailable { warnings, .. } = self;
        record_warnings(warnings);
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let Self::Unavailable { reason, view, .. } = self;
        out.raw_line(0, &format!("unavailable · {reason:?}"))?;
        if let Some(view) = view {
            out.raw_line(
                0,
                &format!("view {} · expires {}", view.id.as_str(), view.expires_at),
            )?;
        }
        Ok(())
    }
}

impl Render for GetSymbolResult {
    fn log_warnings(&mut self) {
        record_warnings(&mut self.warnings);
        for hit in &mut self.hits {
            record_documentation(&mut hit.documentation);
        }
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let Self {
            hits, pagination, ..
        } = self;
        let page = Page { pagination };
        layout::answer(out, &page, hits, symbol::hit)
    }
}

impl Render for NodesResult {
    fn log_warnings(&mut self) {
        record_warnings(&mut self.warnings);
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        nodes::answer(out, self)
    }
}

impl Render for ErrorData {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        error::answer(out, self)
    }
}

impl Render for RegisteredFailure<'_> {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        error::registered(out, self)
    }
}

impl Render for WorkspaceResourcePage {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        workspace::answer(out, self)
    }
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "resource_render_tests.rs"]
mod resource_tests;

impl Render for rift_protocol::symbol_read::GetSymbolResult {
    fn log_warnings(&mut self) {
        match self {
            Self::Found {
                warnings,
                documentation,
                ..
            } => {
                record_warnings(warnings);
                record_documentation(documentation);
            }
            Self::Missing { warnings, .. } | Self::Unavailable { warnings, .. } => {
                record_warnings(warnings);
            }
        }
    }

    fn is_error(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        symbol::canonical(out, self)
    }
}
