//! One record as the line `rift server logs` prints.

use jiff::Timestamp;
use jiff::fmt::temporal::DateTimePrinter;
use jiff::tz::TimeZone;
use serde_json::{Map, Value};

use crate::record::LogRecord;

/// Renders a logged instant with exactly 3 fractional-second digits.
///
/// `DateTimePrinter::new` and `precision` are both `const fn`, so the
/// configured printer is a compile-time value shared by every render.
const TIMESTAMP_PRINTER: DateTimePrinter = DateTimePrinter::new().precision(Some(3));

impl LogRecord {
    /// The record as the operator reads it: when it happened in `time_zone`, how severe
    /// it was, where it came from, what it said, and the fields it carried.
    #[must_use]
    pub fn rendered(&self, time_zone: &TimeZone) -> String {
        let timestamp = rendered_timestamp(self.recorded_at_ms(), time_zone);
        let glyph = level_glyph(self.level());
        let level = self.level().to_uppercase();
        let component = label(self.component());
        let operation = label(self.operation());
        let message = self.message();
        let fields = rendered_fields(self.fields());
        format!("{timestamp} {glyph} {level:<5} {component:<8} {operation:<12} {message}{fields}")
    }
}

/// The glyph one severity prints under. A level outside the five the store
/// records prints under the least severe one.
fn level_glyph(level: &str) -> &'static str {
    match level {
        "error" => "🔴",
        "warn" => "🟡",
        "info" => "🔵",
        "debug" => "⚪",
        _ => "⚫",
    }
}

/// The label a record carried, or `-` when it carried none.
fn label(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

/// The record's remaining fields as ` key=value` pairs, or as the text the
/// store holds when that text is not a JSON object.
fn rendered_fields(fields: &str) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let Ok(named) = serde_json::from_str::<Map<String, Value>>(fields) else {
        return format!(" {fields}");
    };
    let mut fields = named.iter().collect::<Vec<_>>();
    fields.sort_by_key(|(key, _)| *key);
    let mut rendered = String::new();
    for (key, value) in fields {
        rendered.push(' ');
        rendered.push_str(key);
        rendered.push('=');
        rendered.push_str(&rendered_value(value));
    }
    rendered
}

/// One field value without the quotes JSON puts around a string.
fn rendered_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// One recorded instant as an RFC 3339 timestamp in `time_zone`'s local offset.
///
/// Local offset needs the tz database; jiff owns both parsing the recorded
/// millisecond count and rendering it, with exactly 3 fractional digits and
/// a numeric offset - never `Z`, since the offset is always known here. A
/// millisecond count outside jiff's representable range falls back to the
/// raw count instead of panicking.
fn rendered_timestamp(recorded_at_ms: i64, time_zone: &TimeZone) -> String {
    let Ok(timestamp) = Timestamp::from_millisecond(recorded_at_ms) else {
        return recorded_at_ms.to_string();
    };
    let offset = time_zone.to_offset(timestamp);
    TIMESTAMP_PRINTER.timestamp_with_offset_to_string(&timestamp, offset)
}

#[cfg(test)]
mod tests {
    use jiff::tz::{Offset, TimeZone};

    use super::{label, level_glyph, rendered_fields, rendered_timestamp};
    use crate::record::LogRecord;

    #[test]
    fn a_rendered_line_carries_every_column() {
        let record = LogRecord::new(
            1_756_552_944_123,
            "info",
            "rift_mcp::server",
            "index",
            "rebuild",
            "published 412 units",
            "{\"unit_count\":412}",
        );

        assert_eq!(
            record.rendered(&TimeZone::UTC),
            "2025-08-30T11:22:24.123+00:00 🔵 INFO  index    rebuild      \
             published 412 units unit_count=412"
        );
    }

    #[test]
    fn a_record_without_labels_prints_a_dash_in_each_column() {
        let record = LogRecord::new(0, "warn", "rift", "", "", "late", "{}");

        assert_eq!(
            record.rendered(&TimeZone::UTC),
            "1970-01-01T00:00:00.000+00:00 🟡 WARN  -        -            late"
        );
        assert_eq!(label(""), "-");
        assert_eq!(label("index"), "index");
    }

    #[test]
    fn every_level_prints_its_own_glyph() {
        for (level, glyph) in [
            ("error", "🔴"),
            ("warn", "🟡"),
            ("info", "🔵"),
            ("debug", "⚪"),
            ("trace", "⚫"),
            ("loud", "⚫"),
        ] {
            assert_eq!(level_glyph(level), glyph, "{level}");
        }
    }

    #[test]
    fn fields_print_as_pairs_or_as_the_text_the_store_holds() {
        assert_eq!(rendered_fields("{}"), "");
        assert_eq!(rendered_fields(""), "");
        assert_eq!(
            rendered_fields("{\"epoch\":\"4\",\"count\":7}"),
            " count=7 epoch=4"
        );
        assert_eq!(rendered_fields("not json"), " not json");
        assert_eq!(rendered_fields("[1]"), " [1]");
    }

    #[test]
    fn rendered_timestamp_uses_the_given_time_zones_offset() {
        assert_eq!(
            rendered_timestamp(0, &TimeZone::UTC),
            "1970-01-01T00:00:00.000+00:00"
        );
        assert_eq!(
            rendered_timestamp(-1, &TimeZone::UTC),
            "1969-12-31T23:59:59.999+00:00"
        );

        let positive = TimeZone::fixed(Offset::from_hours(2).expect("+2h must be a valid offset"));
        assert_eq!(
            rendered_timestamp(1_756_552_944_123, &positive),
            "2025-08-30T13:22:24.123+02:00"
        );

        let negative = TimeZone::fixed(Offset::from_hours(-5).expect("-5h must be a valid offset"));
        assert_eq!(
            rendered_timestamp(1_756_552_944_123, &negative),
            "2025-08-30T06:22:24.123-05:00"
        );

        let berlin =
            TimeZone::get("Europe/Berlin").expect("the tz database must carry Europe/Berlin");
        assert_eq!(
            rendered_timestamp(1_756_552_944_123, &berlin),
            "2025-08-30T13:22:24.123+02:00",
            "August is daylight saving time in Berlin, CEST"
        );
        assert_eq!(
            rendered_timestamp(1_736_940_144_123, &berlin),
            "2025-01-15T12:22:24.123+01:00",
            "January is standard time in Berlin, CET"
        );
    }

    #[test]
    fn an_out_of_range_millisecond_count_falls_back_to_the_raw_count() {
        assert_eq!(
            rendered_timestamp(i64::MAX, &TimeZone::UTC),
            i64::MAX.to_string()
        );
        assert_eq!(
            rendered_timestamp(i64::MIN, &TimeZone::UTC),
            i64::MIN.to_string()
        );
    }
}
