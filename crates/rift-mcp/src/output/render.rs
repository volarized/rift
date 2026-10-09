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
//! 1 warning
//!   results_truncated · results_max 100
//! ```
//!
//! The title counts the results and names the page when there are several; the page is the only
//! pagination fact. The warnings section follows the items and is left out without warnings. One
//! item has no marker and indents its lines by one level; several items each start with `[n] `
//! at one level and indent their further lines by two. Text an answer copies, such as source,
//! keeps its own whitespace after the indent. No blank line separates two items or two sections.
//! The hits a relationship walk reached are written as a tree; see the `walk` module.
//! `nodes` answers, failures, and workspace pages follow the same principles; see the
//! `nodes`, `error`, and `workspace` modules.

mod error;
mod facts;
mod inline;
mod layout;
mod nodes;
mod search;
mod symbol;
mod walk;
mod warning;
mod workspace;

use rift_protocol::error::ErrorData;
use rift_protocol::read::{GetSymbolResult, NodesResult};
use rift_protocol::search::SearchResult;
use rift_protocol::workspace::WorkspaceResourcePage;

use super::text::{OUTPUT_TEXT_BYTES_MAX, TextError, TextWriter};
use layout::Page;

pub(crate) use error::RegisteredFailure;

/// An answer that writes itself as text.
pub(crate) trait Render {
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
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let Self {
            results,
            pagination,
            warnings,
        } = self;
        let page = Page {
            pagination,
            warnings,
        };
        search::answer(out, &page, results)
    }
}

impl Render for GetSymbolResult {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let Self {
            hits,
            pagination,
            warnings,
        } = self;
        let page = Page {
            pagination,
            warnings,
        };
        layout::answer(out, &page, hits, symbol::hit)
    }
}

impl Render for NodesResult {
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
