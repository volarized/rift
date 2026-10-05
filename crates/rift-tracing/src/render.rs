//! One record as the line stderr and `rift server logs` print.

use std::fmt::Write as _;

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
/// The member of a record's fields that carries the outermost span around it.
const ROOT_SPAN_MEMBER: &str = "root_span";
/// The member of a record's fields that carries the span it was emitted in, when that span
/// is not the outermost.
const NEAREST_SPAN_MEMBER: &str = "nearest_span";
/// The field the root and nearest span print under a shorter label, and that label.
const REQUEST_LABEL: (&str, &str) = ("request_id", "req");
/// The mark before the nearest span of a record nested deeper than the root span.
const NESTED_MARK: &str = "↳";
/// What ends the root and nearest span fields, so the message starts after a wider gap than
/// the one between two of those fields.
const CONTEXT_END: &str = "  ";

/// Whether a rendered line colors its level with ANSI escape codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LevelColor {
    /// Plain text: a file, a pipe, and `rift server logs`.
    Plain,
    /// The level in the color `tracing-subscriber`'s own format gives it on a terminal.
    Ansi,
}

impl LogRecord {
    /// The record as the operator reads it: when it happened in `time_zone`, how severe
    /// it was, its `component` and `operation`, the request or operation it ran inside,
    /// what it said, and the fields it carried.
    ///
    /// Inside a span, the line names the outermost span's fields, then `↳` and the name
    /// and fields of the span it was emitted in, then two spaces before the message. A
    /// span field already printed as the `component` or `operation` column is left out,
    /// and `request_id` prints as `req`.
    #[must_use]
    pub fn rendered(&self, time_zone: &TimeZone) -> String {
        self.rendered_line(time_zone, LevelColor::Plain)
    }

    /// [`Self::rendered`], with the level in `color`.
    pub(crate) fn rendered_line(&self, time_zone: &TimeZone, color: LevelColor) -> String {
        let timestamp = rendered_timestamp(self.recorded_at_ms(), time_zone);
        let level = self.level().to_uppercase();
        let component = label(self.component());
        let operation = label(self.operation());
        let mut line = format!("{timestamp} ");
        match level_color(self.level()).filter(|_| color == LevelColor::Ansi) {
            Some(code) => {
                let _ = write!(line, "\x1b[{code}m{level:<5}\x1b[0m");
            }
            None => {
                let _ = write!(line, "{level:<5}");
            }
        }
        let _ = write!(line, " {component:<8} {operation:<12} ");
        let fields = RecordFields::parsed(self.fields());
        fields.write_context(&mut line, self.component(), self.operation());
        line.push_str(self.message());
        fields.write_own(&mut line);
        line
    }
}

/// The SGR color code `tracing-subscriber` paints one level in on a terminal
/// (`fmt/format/mod.rs`, `FmtLevel`): purple, blue, green, yellow, red. A level outside
/// the five prints plain.
fn level_color(level: &str) -> Option<u8> {
    match level {
        "trace" => Some(35),
        "debug" => Some(34),
        "info" => Some(32),
        "warn" => Some(33),
        "error" => Some(31),
        _ => None,
    }
}

/// The label a record carried, or `-` when it carried none.
fn label(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

/// A record's fields split into the spans it ran inside and its own.
enum RecordFields {
    /// A JSON object: the root and nearest span members apart from the record's own.
    Object {
        root: Option<Map<String, Value>>,
        nearest: Option<Map<String, Value>>,
        own: Map<String, Value>,
    },
    /// Text the store holds that is not a JSON object, printed as it is.
    Text(String),
}

impl RecordFields {
    fn parsed(fields: &str) -> Self {
        if fields.is_empty() {
            return Self::Object {
                root: None,
                nearest: None,
                own: Map::new(),
            };
        }
        let Ok(mut own) = serde_json::from_str::<Map<String, Value>>(fields) else {
            return Self::Text(fields.to_owned());
        };
        let root = span_member(&mut own, ROOT_SPAN_MEMBER);
        let nearest = span_member(&mut own, NEAREST_SPAN_MEMBER);
        Self::Object { root, nearest, own }
    }

    /// Writes the outermost span's fields, `↳` and the nearest span, then the gap before the
    /// message; nothing for a record outside every span.
    fn write_context(&self, line: &mut String, component: &str, operation: &str) {
        let Self::Object { root, nearest, .. } = self else {
            return;
        };
        let before = line.len();
        if let Some(root) = root {
            write_span_fields(line, root, component, operation);
        }
        if let Some(nearest) = nearest {
            if line.len() > before {
                line.push(' ');
            }
            line.push_str(NESTED_MARK);
            if let Some(Value::String(name)) = nearest.get("name") {
                line.push(' ');
                line.push_str(name);
            }
            let length = line.len();
            line.push(' ');
            write_span_fields(line, nearest, component, operation);
            if line.len() == length + 1 {
                line.truncate(length);
            }
        }
        if line.len() > before {
            line.push_str(CONTEXT_END);
        }
    }

    /// Writes the record's own fields as ` key=value` pairs sorted by key, or ` ` and the
    /// stored text.
    fn write_own(&self, line: &mut String) {
        match self {
            Self::Object { own, .. } => {
                let mut fields = own.iter().collect::<Vec<_>>();
                fields.sort_by_key(|(key, _)| *key);
                for (key, value) in fields {
                    line.push(' ');
                    write_pair(line, key, value);
                }
            }
            Self::Text(text) => {
                line.push(' ');
                line.push_str(text);
            }
        }
    }
}

/// Takes the span object under `member` out of `fields`, when it is one.
fn span_member(fields: &mut Map<String, Value>, member: &str) -> Option<Map<String, Value>> {
    match fields.remove(member)? {
        Value::Object(span) => Some(span),
        other => {
            fields.insert(member.to_owned(), other);
            None
        }
    }
}

/// Writes the `fields` of one span object as `key=value` pairs separated by one space:
/// `component`, `operation`, then the rest sorted by key. A `component` or `operation`
/// equal to the record's column is left out.
fn write_span_fields(
    line: &mut String,
    span: &Map<String, Value>,
    component: &str,
    operation: &str,
) {
    let Some(Value::Object(fields)) = span.get("fields") else {
        return;
    };
    let start = line.len();
    let separate = |line: &mut String| {
        if line.len() > start {
            line.push(' ');
        }
    };
    for (key, column) in [("component", component), ("operation", operation)] {
        match fields.get(key) {
            Some(Value::String(value)) if value == column => {}
            Some(value) => {
                separate(line);
                write_pair(line, key, value);
            }
            None => {}
        }
    }
    let mut rest = fields
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "component" | "operation"))
        .collect::<Vec<_>>();
    rest.sort_by_key(|(key, _)| *key);
    for (key, value) in rest {
        separate(line);
        let key = if key == REQUEST_LABEL.0 {
            REQUEST_LABEL.1
        } else {
            key
        };
        write_pair(line, key, value);
    }
}

/// Writes `key=value`, a string value without the quotes JSON puts around it.
fn write_pair(line: &mut String, key: &str, value: &Value) {
    line.push_str(key);
    line.push('=');
    match value {
        Value::String(text) => line.push_str(text),
        other => {
            let _ = write!(line, "{other}");
        }
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

    use super::{LevelColor, label, level_color, rendered_timestamp};
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
            "2025-08-30T11:22:24.123+00:00 INFO  index    rebuild      \
             published 412 units unit_count=412"
        );
    }

    #[test]
    fn a_record_without_labels_prints_a_dash_in_each_column() {
        let record = LogRecord::new(0, "warn", "rift", "", "", "late", "{}");

        assert_eq!(
            record.rendered(&TimeZone::UTC),
            "1970-01-01T00:00:00.000+00:00 WARN  -        -            late"
        );
        assert_eq!(label(""), "-");
        assert_eq!(label("index"), "index");
    }

    /// The root span's fields print before the message, a field the columns already show
    /// left out and `request_id` as `req`; the nearest span follows `↳`.
    #[test]
    fn a_record_inside_two_spans_prints_the_root_and_the_nearest_span() {
        let record = LogRecord::new(
            0,
            "debug",
            "rift_index::workspace",
            "index",
            "fingerprint.discover",
            "walked the workspace",
            "{\"files\":\"12\",\"root_span\":{\"name\":\"mcp.request\",\"fields\":\
             {\"component\":\"mcp\",\"operation\":\"tools/call\",\"request_id\":\"18\",\
             \"tool\":\"get_symbol\"}},\"nearest_span\":{\"name\":\"fingerprint.discover\",\
             \"fields\":{\"component\":\"index\",\"operation\":\"fingerprint.discover\"}}}",
        );

        assert_eq!(
            record.rendered(&TimeZone::UTC),
            "1970-01-01T00:00:00.000+00:00 DEBUG index    fingerprint.discover \
             component=mcp operation=tools/call req=18 tool=get_symbol ↳ fingerprint.discover  \
             walked the workspace files=12"
        );
    }

    /// A `root_span` that is not an object is a field of the record like any other.
    #[test]
    fn a_span_member_that_is_not_an_object_prints_as_a_field() {
        let record = LogRecord::new(0, "info", "rift", "mcp", "", "odd", "{\"root_span\":\"7\"}");

        assert_eq!(
            record.rendered(&TimeZone::UTC),
            "1970-01-01T00:00:00.000+00:00 INFO  mcp      -            odd root_span=7"
        );
    }

    #[test]
    fn a_terminal_line_colors_the_level_alone() {
        let record = LogRecord::new(0, "warn", "rift", "mcp", "server.stop", "late", "{}");

        assert_eq!(
            record.rendered_line(&TimeZone::UTC, LevelColor::Ansi),
            "1970-01-01T00:00:00.000+00:00 \u{1b}[33mWARN \u{1b}[0m mcp      server.stop  late"
        );
        for (level, code) in [
            ("error", Some(31)),
            ("warn", Some(33)),
            ("info", Some(32)),
            ("debug", Some(34)),
            ("trace", Some(35)),
            ("loud", None),
        ] {
            assert_eq!(level_color(level), code, "{level}");
        }
    }

    #[test]
    fn fields_print_as_pairs_or_as_the_text_the_store_holds() {
        let line = |fields: &str| {
            LogRecord::new(0, "info", "rift", "a", "b", "m", fields).rendered(&TimeZone::UTC)
        };
        let prefix = "1970-01-01T00:00:00.000+00:00 INFO  a        b            m";
        assert_eq!(line("{}"), prefix);
        assert_eq!(line(""), prefix);
        assert_eq!(
            line("{\"epoch\":\"4\",\"count\":7}"),
            format!("{prefix} count=7 epoch=4")
        );
        assert_eq!(line("not json"), format!("{prefix} not json"));
        assert_eq!(line("[1]"), format!("{prefix} [1]"));
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
