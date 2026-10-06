//! Text of the `rift://logs` resource: one line per recorded diagnostic, newest first.
//!
//! [`LogsPage`] borrows the records a read selected. The resource builds its JSON body from the
//! same page, so the text and the JSON state the same records.

use jiff::Timestamp;
use serde_json::{Map, Value};

use super::facts::{DETAIL_SEPARATOR, FACT_SEPARATOR, quoted_value};
use super::{layout, warning};
use crate::output::text::{TextError, TextWriter};

/// `YYYY-MM-DD HH:MM:SS.mmm`, in jiff's `strftime` directives.
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";
/// Written for a component or an operation a record did not carry.
const NO_LABEL: &str = "-";
/// Noun counted by the title of the records section.
const RECORD_NOUN: &str = "record";

/// The extra fields of one record.
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
    /// Milliseconds since the Unix epoch at which the record was emitted.
    pub(crate) recorded_at_ms: i64,
    /// Severity in lower case.
    pub(crate) level: &'a str,
    /// The emitting module path.
    pub(crate) target: &'a str,
    /// The emitting component, empty when the record named none.
    pub(crate) component: &'a str,
    /// The emitting operation, empty when the record named none.
    pub(crate) operation: &'a str,
    /// The record's message.
    pub(crate) message: &'a str,
    /// The record's remaining fields.
    pub(crate) fields: LogFields<'a>,
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
pub(super) fn answer(out: &mut TextWriter, page: &LogsPage<'_>) -> Result<(), TextError> {
    let LogsPage {
        records,
        unavailable,
    } = page;
    layout::title(out, records.len(), RECORD_NOUN, None)?;
    for record in records {
        layout::entry(out, 0, &line(record))?;
    }
    let reasons: Vec<String> = unavailable
        .iter()
        .map(|reason| format!("unavailable{DETAIL_SEPARATOR}{}", quoted_value(reason)))
        .collect();
    warning::lines_section(out, &reasons)
}

/// `time level component operation[: message][ · key value]...`.
///
/// A message or a field value that holds ` · ` or `: ` is quoted, with `"` and `\` escaped
/// inside the quotes.
fn line(record: &LogLine<'_>) -> String {
    let LogLine {
        identity: _,
        recorded_at_ms,
        level,
        target: _,
        component,
        operation,
        message,
        fields,
    } = record;
    let mut text = format!(
        "{} {level} {} {}",
        utc_time(*recorded_at_ms),
        label(component),
        label(operation)
    );
    if !message.is_empty() {
        text.push_str(DETAIL_SEPARATOR);
        text.push_str(&quoted_value(message));
    }
    push_fields(&mut text, fields);
    text
}

/// The label a record carried, or `-` when it carried none.
fn label(value: &str) -> &str {
    if value.is_empty() { NO_LABEL } else { value }
}

/// Appends each entry of an object as ` · key value`, or non-empty text as ` · text`.
fn push_fields(text: &mut String, fields: &LogFields<'_>) {
    match fields {
        LogFields::Object(object) => {
            for (key, value) in object {
                text.push_str(FACT_SEPARATOR);
                text.push_str(key);
                text.push(' ');
                text.push_str(&quoted_value(&value_text(value)));
            }
        }
        LogFields::Text(rest) if !rest.is_empty() => {
            text.push_str(FACT_SEPARATOR);
            text.push_str(&quoted_value(rest));
        }
        LogFields::Text(_) => {}
    }
}

/// A string as it is, an empty string as `""`, any other value as compact JSON.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) if text.is_empty() => "\"\"".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `YYYY-MM-DD HH:MM:SS.mmm` in UTC.
///
/// A count outside the range of [`Timestamp`] is written as the count itself.
pub(super) fn utc_time(recorded_at_ms: i64) -> String {
    Timestamp::from_millisecond(recorded_at_ms).map_or_else(
        |_| recorded_at_ms.to_string(),
        |timestamp| timestamp.strftime(TIME_FORMAT).to_string(),
    )
}
