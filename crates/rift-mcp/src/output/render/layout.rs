//! Counted sections of an answer, and the lines of one item.
//!
//! An answer is a sequence of sections. A section is one title line at column 0, then its
//! entries, each indented by one level. One level is one indent unit of the writer, a tab. A
//! counted title is `N noun`, in the singular for one.
//! No blank line separates a title from its entries, two entries, or two sections.
//!
//! The items of a tool answer are the entries of its first section. One item has no marker and
//! indents every line by one level. Several items each start with `[n] ` at one level and
//! indent their further lines by two. Single-line facts are written with control characters made
//! visible; multi-line text is written line by line, bytes unchanged.
//!
//! The hits a relationship walk reached are the nodes of a tree, one entry of the section. A
//! node under the root starts with a lead such as `↳ called`, one level deeper than the lines
//! of its parent, and indents its further lines by one more.

use std::fmt::Display;

use rift_protocol::read::{Pagination, ReadWarning};

use super::facts::FACT_SEPARATOR;
use super::warning;
use crate::output::text::{TextError, TextWriter, visible};

/// Levels that indent an entry of a section.
const SECTION_INDENT: usize = 1;
/// Levels, past the section indent, that indent the further lines of one item among several.
const MARKER_INDENT: usize = 1;
/// Levels, past the head of a node under the root, that indent its further lines and the heads
/// of the nodes under it.
const NODE_INDENT: usize = 1;
/// Noun counted by the title of the items of `search` and `get_symbol`.
const RESULT_NOUN: &str = "result";

/// Where an answer sits among its pages, and the warnings it carries.
pub(super) struct Page<'a> {
    /// Page index and page count of the answer.
    pub(super) pagination: &'a Pagination,
    /// Warnings written in their own section after the items.
    pub(super) warnings: &'a [ReadWarning],
}

/// The lines of one item, indented as its place in the answer requires.
pub(super) struct Lines<'a> {
    out: &'a mut TextWriter,
    indent: usize,
    /// Levels that indent the first line of the item.
    head_indent: usize,
    /// Text written before the first line: the marker of an item among several, or the lead of
    /// a tree node.
    lead: Option<String>,
}

impl<'a> Lines<'a> {
    /// The lines of the item at `index` among `count` items.
    pub(super) fn new(out: &'a mut TextWriter, index: usize, count: usize) -> Self {
        let lead = (count > 1).then(|| format!("[{}] ", index.saturating_add(1)));
        Self {
            out,
            indent: item_indent(count),
            head_indent: SECTION_INDENT,
            lead,
        }
    }

    /// The lines of a tree node `depth` levels under the root, behind `lead` when it has one.
    ///
    /// The root indents every line like a single item. A node under it starts its head one level
    /// deeper than the lines of its parent and indents its further lines by one more.
    pub(super) fn node(out: &'a mut TextWriter, depth: usize, lead: Option<String>) -> Self {
        let head_indent = SECTION_INDENT.saturating_add(NODE_INDENT.saturating_mul(depth));
        let indent = if depth == 0 {
            head_indent
        } else {
            head_indent.saturating_add(NODE_INDENT)
        };
        Self {
            out,
            indent,
            head_indent,
            lead,
        }
    }

    /// Lines that continue the last of `count` items after its head and facts.
    pub(super) fn after_last(out: &'a mut TextWriter, count: usize) -> Self {
        Self {
            out,
            indent: item_indent(count),
            head_indent: SECTION_INDENT,
            lead: None,
        }
    }

    /// Writes the first line of the item, behind its marker or lead when it has one.
    ///
    /// Empty text writes no line for an item without either, and only the marker or lead for
    /// one that has it.
    pub(super) fn head(&mut self, text: &str) -> Result<(), TextError> {
        let text = visible(text);
        let text = text.trim_end_matches(' ');
        match self.lead.take() {
            Some(lead) => self
                .out
                .raw_line(self.head_indent, format!("{lead}{text}").trim_end()),
            None if text.is_empty() => Ok(()),
            None => self.out.raw_line(self.head_indent, text),
        }
    }

    /// Writes one single-line fact `extra` levels deeper than the item. Empty text writes nothing.
    pub(super) fn line(&mut self, extra: usize, text: &str) -> Result<(), TextError> {
        let text = visible(text);
        let text = text.trim_end_matches(' ');
        if text.is_empty() {
            return Ok(());
        }
        self.out.raw_line(self.indent.saturating_add(extra), text)
    }

    /// Writes one empty line.
    pub(super) fn blank(&mut self) -> Result<(), TextError> {
        self.out.blank_line()
    }

    /// Writes multi-line text line by line, `extra` levels deeper than the item, bytes
    /// unchanged.
    ///
    /// Empty text writes one empty line. Blank lines stay empty.
    pub(super) fn verbatim(&mut self, extra: usize, text: &str) -> Result<(), TextError> {
        self.out.raw_lines(self.indent.saturating_add(extra), text)
    }
}

/// Levels that indent the lines of an item among `count` items, its head excepted.
fn item_indent(count: usize) -> usize {
    if count > 1 {
        SECTION_INDENT.saturating_add(MARKER_INDENT)
    } else {
        SECTION_INDENT
    }
}

/// Writes a `search` or `get_symbol` answer: the results section, then the warnings section.
///
/// `item` writes one item through the [`Lines`] it is given and starts with [`Lines::head`].
pub(super) fn answer<T>(
    out: &mut TextWriter,
    page: &Page<'_>,
    results: &[T],
    item: impl Fn(&mut Lines<'_>, &T) -> Result<(), TextError>,
) -> Result<(), TextError> {
    answer_of(out, page, results.len(), |out| items(out, results, item))
}

/// Writes the results section of `count` results through `entries`, then the warnings section.
pub(super) fn answer_of(
    out: &mut TextWriter,
    page: &Page<'_>,
    count: usize,
    entries: impl FnOnce(&mut TextWriter) -> Result<(), TextError>,
) -> Result<(), TextError> {
    let page_fact = page_fact(page.pagination);
    title(out, count, RESULT_NOUN, page_fact.as_deref())?;
    entries(out)?;
    warning::section(out, page.warnings)
}

/// Writes each item under the title, with markers when there are several.
pub(super) fn items<T>(
    out: &mut TextWriter,
    items: &[T],
    item: impl Fn(&mut Lines<'_>, &T) -> Result<(), TextError>,
) -> Result<(), TextError> {
    for (index, entry) in items.iter().enumerate() {
        item(&mut Lines::new(out, index, items.len()), entry)?;
    }
    Ok(())
}

/// Writes the title of a counted section, then `fact` after the count when there is one.
pub(super) fn title(
    out: &mut TextWriter,
    count: usize,
    noun: &str,
    fact: Option<&str>,
) -> Result<(), TextError> {
    let counted = counted(count, noun);
    match fact {
        Some(fact) => out.raw_line(0, &format!("{counted}{FACT_SEPARATOR}{fact}")),
        None => out.raw_line(0, &counted),
    }
}

/// Writes one single-line entry of a section, `depth` levels under the section entries.
///
/// Control characters are made visible. Empty text writes nothing.
pub(super) fn entry(out: &mut TextWriter, depth: usize, text: &str) -> Result<(), TextError> {
    let text = visible(text);
    let text = text.trim_end_matches(' ');
    if text.is_empty() {
        return Ok(());
    }
    out.raw_line(SECTION_INDENT.saturating_mul(depth.saturating_add(1)), text)
}

/// The count of `count` things named `noun`, in the singular for one.
pub(super) fn counted<N: Copy + Display + PartialEq + From<u8>>(count: N, noun: &str) -> String {
    let plural = if count == N::from(1) { "" } else { "s" };
    format!("{count} {noun}{plural}")
}

/// `page P/T` when the answer has several pages or lies past the last, else nothing.
pub(super) fn page_fact(pagination: &Pagination) -> Option<String> {
    let Pagination {
        page_index,
        total_pages,
    } = pagination;
    let past_the_end = *total_pages > 0 && page_index >= total_pages;
    (*total_pages > 1 || past_the_end)
        .then(|| format!("page {}/{total_pages}", page_index.saturating_add(1)))
}

/// The named sections of a resource, directly under its header line.
pub(super) struct Sections<'a> {
    out: &'a mut TextWriter,
}

impl<'a> Sections<'a> {
    pub(super) fn new(out: &'a mut TextWriter) -> Self {
        Self { out }
    }

    /// Writes `title:` on its own line. Callers open a section only when it has entries.
    pub(super) fn open(&mut self, title: &str) -> Result<(), TextError> {
        self.out.raw_line(0, &format!("{title}:"))
    }

    /// Writes one single-line entry `depth` levels under the section entries.
    ///
    /// Control characters are made visible. Empty text writes nothing.
    pub(super) fn entry(&mut self, depth: usize, text: &str) -> Result<(), TextError> {
        entry(self.out, depth, text)
    }

    /// Writes the warnings section after the last named section.
    pub(super) fn warnings(&mut self, warnings: &[ReadWarning]) -> Result<(), TextError> {
        warning::section(self.out, warnings)
    }
}
