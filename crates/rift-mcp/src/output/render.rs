//! Text of tool answers and execution errors, written through [`TextWriter`].
//!
//! Each answer type implements [`Render`]. [`text_of`] renders an answer under a byte limit.
//! Search writes a plain-text table with its row range and page. An exact symbol read opens
//! with its canonical identity, then declaration facets and the selected sections.
//! Text copied from a declaration keeps its own whitespace after the layout indent.
//! Other tool and resource layouts live in the modules below.

mod error;
mod facets;
mod facts;
mod layout;
mod map;
mod query;
mod search;
mod symbol;
mod unavailable;
mod walk;
mod workspace;

use rift_protocol::error::ErrorData;
use rift_protocol::read::{FacetsResult, GetSymbolResult};
use rift_protocol::search::SearchResult;
use rift_protocol::workspace::WorkspaceResourcePage;

use super::text::{OUTPUT_TEXT_BYTES_MAX, TextError, TextWriter};
use super::warnings::{record_documentation, record_warnings};
use layout::Page;

pub(crate) use error::RegisteredFailure;
pub(crate) use query::{QueryDisplay, QueryOutput};

/// An answer that writes itself as text.
pub(crate) trait Render {
    /// Whether this typed answer reports a failed tool call.
    fn is_error(&self) -> bool {
        false
    }

    /// Whether this result carries a text block.
    fn has_text(&self) -> bool {
        true
    }

    /// Records read warnings before serialization to a client.
    fn log_warnings(&mut self) {}

    /// Keeps presentation context available to a proxy that qualifies the structured answer.
    fn metadata(&self) -> Result<Option<rmcp::model::MetaObject>, serde_json::Error> {
        Ok(None)
    }

    /// Writes this answer into `out`.
    ///
    /// # Errors
    ///
    /// Returns the first failure of `out`: overflow, an unsupported shape, or a `Serialize`
    /// failure.
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError>;
}

impl Render for rift_protocol::feedback::FeedbackResult {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        out.raw_line(0, "Feedback submitted.")
    }
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
                &format!("view {} | expires {}", view.id.as_str(), view.expires_at),
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
        out.raw_line(0, &unavailable::line(*reason))?;
        if let Some(view) = view {
            out.raw_line(
                0,
                &format!("view {} | expires {}", view.id.as_str(), view.expires_at),
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

impl Render for FacetsResult {
    fn log_warnings(&mut self) {
        record_warnings(&mut self.warnings);
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        facets::answer(out, self)
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

impl Render for rift_protocol::map::WorkspaceMap {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        map::answer(out, self)
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

impl Render for rift_protocol::mcp_symbol::GetSymbolResult {
    fn log_warnings(&mut self) {
        match self {
            Self::Found(found) => {
                record_warnings(&mut found.warnings);
                record_documentation(&mut found.documentation);
            }
            Self::Missing { warnings, .. } | Self::Unavailable { warnings, .. } => {
                record_warnings(warnings);
            }
        }
    }

    fn has_text(&self) -> bool {
        !matches!(self, Self::Missing { .. })
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        symbol::canonical(out, self)
    }
}
