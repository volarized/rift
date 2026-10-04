//! Text of the `rift://logs` resource: one line per recorded diagnostic, newest first.
//!
//! [`LogsPage`] borrows the records a read selected. The resource builds its JSON body from the
//! same page, so the text and the JSON state the same records.

use serde_json::{Map, Value};

use super::facts::FACT_SEPARATOR;
use super::{layout, warning};
use crate::output::text::{TextError, TextWriter};

/// Milliseconds in one day.
const MILLIS_PER_DAY: i64 = 86_400_000;
/// The last millisecond year 9999 holds. A later or earlier time is written as its count.
const TIME_MILLIS_MAX: i64 = 253_402_300_799_999;
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
        .map(|reason| format!("unavailable: {reason}"))
        .collect();
    warning::lines_section(out, &reasons)
}

/// `time level component operation[: message][ · key value]...`.
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
        text.push_str(": ");
        text.push_str(message);
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
                text.push_str(&value_text(value));
            }
        }
        LogFields::Text(rest) if !rest.is_empty() => {
            text.push_str(FACT_SEPARATOR);
            text.push_str(rest);
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
/// A count before the epoch or after the end of year 9999 is written as the count itself.
pub(super) fn utc_time(recorded_at_ms: i64) -> String {
    if !(0..=TIME_MILLIS_MAX).contains(&recorded_at_ms) {
        return recorded_at_ms.to_string();
    }
    let (year, month, day) = civil_date(recorded_at_ms / MILLIS_PER_DAY);
    let of_day = recorded_at_ms % MILLIS_PER_DAY;
    let (hour, minute) = (of_day / 3_600_000, of_day / 60_000 % 60);
    let (second, milli) = (of_day / 1_000 % 60, of_day % 1_000);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{milli:03}")
}

/// The proleptic Gregorian date `days` after 1970-01-01, as year, month, and day.
///
/// Days are counted from March 1 of a 400-year era, so the leap day closes the year.
fn civil_date(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}
