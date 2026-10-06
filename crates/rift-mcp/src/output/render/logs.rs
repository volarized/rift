//! Text of the `rift://logs` resource: the records a read selected, newest first, as the lines
//! `rift-tracing`'s [`LogLines`] prints for a stored page.
//!
//! [`LogsPage`] borrows the records a read selected. The resource builds its JSON body from the
//! same page, so the text and the JSON state the same records.

use rift_tracing::{LogLines, LogRecord};
use serde_json::{Map, Value};

use super::facts::{DETAIL_SEPARATOR, quoted_value};
use super::{layout, warning};
use crate::output::text::{TextError, TextWriter};

/// Noun counted by the title of the records section.
const RECORD_NOUN: &str = "record";

/// The extra fields of one record, as the JSON body carries them.
#[derive(Debug, PartialEq)]
pub(crate) enum LogFields<'a> {
    /// The fields rendered as a JSON object, parsed once.
    Object(Map<String, Value>),
    /// Fields that do not parse as a JSON object, as the store holds them.
    Text(&'a str),
}

impl<'a> LogFields<'a> {
    /// The object `text` holds, or `text` itself when it is not a JSON object.
    pub(crate) fn parse(text: &'a str) -> Self {
        serde_json::from_str::<Map<String, Value>>(text).map_or(Self::Text(text), Self::Object)
    }

    /// The fields as the JSON wire carries them: the object, or the text as a string.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Object(object) => Value::Object(object.clone()),
            Self::Text(text) => Value::String((*text).to_owned()),
        }
    }
}

/// One recorded diagnostic, borrowed from the store's record.
#[derive(Debug, PartialEq)]
pub(crate) struct LogLine<'a> {
    /// The store's own ascending number for the record.
    pub(crate) identity: i64,
    /// The record itself.
    pub(crate) record: &'a LogRecord,
}

/// The records one read selected, or the reason the store holds none.
#[derive(Debug, PartialEq)]
pub(crate) struct LogsPage<'a> {
    /// The records, newest first.
    pub(crate) records: Vec<LogLine<'a>>,
    /// Why the log store could not be read. The records are empty then.
    pub(crate) unavailable: Option<&'a str>,
}

/// Writes the records section, then a warnings section that holds the unavailable reason.
///
/// The records section holds the lines [`LogLines::stored_page`] prints for the records, in the
/// order of the page, each indented by one level; the blank line between two groups stays empty.
pub(super) fn answer(out: &mut TextWriter, page: &LogsPage<'_>) -> Result<(), TextError> {
    let LogsPage {
        records,
        unavailable,
    } = page;
    layout::title(out, records.len(), RECORD_NOUN, None)?;
    let lines = LogLines::stored_page().lines(records.iter().map(|line| line.record));
    for line in lines.lines() {
        if line.is_empty() {
            out.blank_line()?;
        } else {
            layout::entry(out, 0, line)?;
        }
    }
    let reasons: Vec<String> = unavailable
        .iter()
        .map(|reason| format!("unavailable{DETAIL_SEPARATOR}{}", quoted_value(reason)))
        .collect();
    warning::lines_section(out, &reasons)
}
