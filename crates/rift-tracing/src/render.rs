//! One record as the line stderr, `rift server logs`, and a failure window print.
//!
//! A line holds, in order: the time in UTC, the level, the function, the context, the
//! nested operation, and the message.
//!
//! ```text
//! 2026-10-04 20:42:58.798Z INFO  rift_mcp::server::RiftMcp::nodes   component=mcp operation=tools/call req=11 tool=nodes  ↳ fingerprint.fold component=index operation=fingerprint.fold close ✓ busy=12.1µs idle=13.2µs
//! ```
//!
//! - The function is the `code.function.name` of the root span, the outermost span around
//!   the record; outside every span, the record's own; for a metric snapshot record, the
//!   instrument group. A record that carries none prints its target.
//! - The context is the root span's fields: `component`, `operation`, then the rest sorted
//!   by key, `request_id` under the label `req`. A record outside every span prints its
//!   own `component`, `operation`, and fields there instead.
//! - The nested operation is `↳`, the nearest span's name, and its fields, when the
//!   record belongs to a span below the root.
//! - The message is `close`, a mark, the reason the operation did not complete, then
//!   `busy` and `idle` for a span close; a mark and the message for a lifecycle record;
//!   the values for a metric snapshot record; the message and the record's own fields
//!   otherwise.
//!
//! A group is a run of consecutive records of one request; without a request, of one
//! root span; without a span, of one function. Each metric snapshot record is a group of
//! its own. A blank line separates two groups. A stored page pads each column to the
//! widest value of its group; a live stream, which does not know a group ahead, pads to
//! fixed widths.

use std::fmt::Write as _;

use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::capture::{OUTCOME_FIELD, completed_outcome};
use crate::record::{LogRecord, RecordKind};

/// The time a line prints: UTC with milliseconds, `2026-10-04 20:42:58.787Z`.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3fZ";
/// The member of a record's fields that carries the outermost span around it.
const ROOT_SPAN_MEMBER: &str = "root_span";
/// The member of a record's fields that carries the span it was emitted in, when that span
/// is not the outermost.
const NEAREST_SPAN_MEMBER: &str = "nearest_span";
/// The field that names the function that opened a span or emitted an event.
const FUNCTION_FIELD: &str = "code.function.name";
/// The field the context prints under a shorter label, and that label.
const REQUEST_LABEL: (&str, &str) = ("request_id", "req");
/// The members a span close record adds to the span's own fields.
const CLOSE_MEMBERS: [&str; 6] = [
    "span",
    "elapsed_ms",
    "busy_ns",
    "idle_ns",
    "status.code",
    "error.type",
];
/// The mark before the nearest span of a record nested below the root span.
const NESTED_MARK: &str = "↳";
/// The mark of a phase that starts.
const STARTED_MARK: &str = "→";
/// The mark of an operation or a phase that completed.
const COMPLETED_MARK: &str = "✓";
/// The mark of an operation that failed, panicked, or was cancelled, or a phase that failed.
const FAILED_MARK: &str = "✗";
/// What follows the function column.
const FUNCTION_END: &str = "   ";
/// What follows the context column.
const CONTEXT_END: &str = "  ";
/// The width a live stream pads the function column to.
const LIVE_FUNCTION_WIDTH: usize = 36;
/// The width a live stream pads the context column to.
const LIVE_CONTEXT_WIDTH: usize = 48;
/// The width a live stream pads the nearest span's name to.
const LIVE_NESTED_NAME_WIDTH: usize = 24;
/// The width a live stream pads the nearest span's fields to.
const LIVE_NESTED_FIELDS_WIDTH: usize = 40;

/// Whether a rendered line colors its level with ANSI escape codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LevelColor {
    /// Plain text: a file, a pipe, and `rift server logs`.
    Plain,
    /// The level in the color `tracing-subscriber`'s own format gives it on a terminal.
    Ansi,
}

impl LogRecord {
    /// The record as one line of a live stream: its columns padded to the fixed widths,
    /// without a trailing newline.
    #[must_use]
    pub fn rendered(&self) -> String {
        self.rendered_line(LevelColor::Plain)
    }

    /// [`Self::rendered`], with the level in `color`.
    pub(crate) fn rendered_line(&self, color: LevelColor) -> String {
        LineParts::of(self).line(&Widths::LIVE, color)
    }
}

/// Prints records as lines: one record per line, a blank line between two groups.
///
/// A stored page, `rift server logs` and a failure window, holds every record of a call
/// and pads each column to the widest value of its group; a live stream, stderr and
/// `rift server logs --follow`, pads to fixed widths. Both remember the group of the last
/// line they printed, so the records of one run printed over several calls break where
/// the group changes and nowhere else.
#[derive(Debug)]
pub struct LogLines {
    padding: Padding,
    color: LevelColor,
    previous: Option<Group>,
    printed: bool,
}

/// How [`LogLines`] pads the columns of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Padding {
    /// To the widest value of the group.
    Group,
    /// To the fixed widths of a live stream.
    Live,
}

impl LogLines {
    /// Lines of a stored page: every column padded to the widest value of its group.
    #[must_use]
    pub const fn stored_page() -> Self {
        Self::new(Padding::Group, LevelColor::Plain)
    }

    /// Lines of a live stream: every column padded to a fixed width.
    #[must_use]
    pub const fn live_stream() -> Self {
        Self::new(Padding::Live, LevelColor::Plain)
    }

    const fn new(padding: Padding, color: LevelColor) -> Self {
        Self {
            padding,
            color,
            previous: None,
            printed: false,
        }
    }

    /// The lines of `records`, oldest first, each ending in a newline, with a blank line
    /// wherever the group changes, including against the last record of the previous call.
    #[must_use]
    pub fn lines(&mut self, records: &[LogRecord]) -> String {
        let parts = records.iter().map(LineParts::of).collect::<Vec<_>>();
        let mut text = String::new();
        let mut start = 0;
        while start < parts.len() {
            let first = parts[start].group.as_ref();
            let end = (start + 1..parts.len())
                .find(|index| !Group::same(first, parts[*index].group.as_ref()))
                .unwrap_or(parts.len());
            let group = &parts[start..end];
            let widths = match self.padding {
                Padding::Live => Widths::LIVE,
                Padding::Group => Widths::of(group),
            };
            if self.printed && !Group::same(self.previous.as_ref(), group[0].group.as_ref()) {
                text.push('\n');
            }
            for part in group {
                text.push_str(&part.line(&widths, self.color));
                text.push('\n');
            }
            self.previous.clone_from(&group[group.len() - 1].group);
            self.printed = true;
            start = end;
        }
        text
    }
}

/// One record as a line of a live stream, rendered apart from the stream it joins, so
/// many threads render at once and the stream is held only to place the line.
#[derive(Debug)]
pub(crate) struct LiveLine {
    group: Option<Group>,
    line: String,
}

impl LiveLine {
    /// `record` as a live stream line, the level in `color`.
    pub(crate) fn of(record: &LogRecord, color: LevelColor) -> Self {
        let parts = LineParts::of(record);
        let mut line = parts.line(&Widths::LIVE, color);
        line.push('\n');
        Self {
            group: parts.group,
            line,
        }
    }
}

impl LogLines {
    /// The text that places `line` in the stream: the line, after a blank line when its
    /// group differs from the line before.
    pub(crate) fn placed(&mut self, line: LiveLine) -> String {
        let LiveLine { group, mut line } = line;
        if self.printed && !Group::same(self.previous.as_ref(), group.as_ref()) {
            line.insert(0, '\n');
        }
        self.previous = group;
        self.printed = true;
        line
    }
}

/// What makes consecutive records one group.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Group {
    /// The records of one request: the root span's `request_id`.
    Request(String),
    /// The records of one root span without a request: its name and context.
    Span(String),
    /// The records outside every span, of one function.
    Function(String),
}

impl Group {
    /// Whether two records of these groups belong to one: a record without a group, a
    /// metric snapshot record, belongs to none but its own.
    fn same(first: Option<&Self>, second: Option<&Self>) -> bool {
        matches!((first, second), (Some(first), Some(second)) if first == second)
    }
}

/// The widths one line pads its columns to.
#[derive(Clone, Copy, Debug)]
struct Widths {
    function: usize,
    context: usize,
    nested_name: usize,
    nested_fields: usize,
}

impl Widths {
    const LIVE: Self = Self {
        function: LIVE_FUNCTION_WIDTH,
        context: LIVE_CONTEXT_WIDTH,
        nested_name: LIVE_NESTED_NAME_WIDTH,
        nested_fields: LIVE_NESTED_FIELDS_WIDTH,
    };

    /// The widest value of each column in `group`.
    fn of(group: &[LineParts]) -> Self {
        let width = |text: &str| text.chars().count();
        let mut widths = Self {
            function: 0,
            context: 0,
            nested_name: 0,
            nested_fields: 0,
        };
        for part in group {
            widths.function = widths.function.max(width(&part.function));
            widths.context = widths.context.max(width(&part.context));
            if let Some((name, fields)) = &part.nested {
                widths.nested_name = widths.nested_name.max(width(name));
                widths.nested_fields = widths.nested_fields.max(width(fields));
            }
        }
        widths
    }
}

/// One record's columns, each escaped, before padding.
#[derive(Debug)]
struct LineParts {
    timestamp: String,
    level: String,
    color: Option<u8>,
    function: String,
    context: String,
    nested: Option<(String, String)>,
    message: String,
    group: Option<Group>,
}

impl LineParts {
    /// The columns of `record`.
    fn of(record: &LogRecord) -> Self {
        let mut parts = Self {
            timestamp: rendered_timestamp(record.recorded_at_ms()),
            level: escaped(&record.level().to_uppercase()),
            color: level_color(record.level()),
            function: String::new(),
            context: String::new(),
            nested: None,
            message: String::new(),
            group: None,
        };
        if record.kind() == RecordKind::Metric {
            parts.function = escaped(label(record.operation()));
            parts.message = own_pairs(&parsed_fields(record.fields()).unwrap_or_default());
            return parts;
        }
        let Some(mut own) = parsed_fields(record.fields()) else {
            parts.function = escaped(record.target());
            parts.context = labels_context(record, &Map::new());
            parts.message = escaped(record.message());
            push_separated(&mut parts.message, &escaped(record.fields()));
            parts.group = Some(Group::Function(parts.function.clone()));
            return parts;
        };
        let root = span_member(&mut own, ROOT_SPAN_MEMBER);
        let nearest = span_member(&mut own, NEAREST_SPAN_MEMBER);
        let closed = own.get("span").and_then(Value::as_str) == Some("closed");
        let function = |fields: &Map<String, Value>| {
            fields
                .get(FUNCTION_FIELD)
                .and_then(Value::as_str)
                .map_or_else(|| escaped(record.target()), escaped)
        };
        match (&root, closed) {
            (Some(root), _) => {
                let root_fields = span_fields(root);
                parts.function = function(&root_fields);
                parts.context = context_pairs(&root_fields);
                parts.group = Some(span_group(root, &root_fields));
            }
            (None, true) => {
                parts.function = function(&own);
                parts.context = labels_context(record, &span_own(&own));
                let name = escaped(record.message());
                parts.group = Some(match own.get(REQUEST_LABEL.0) {
                    Some(request) => Group::Request(plain_value(request)),
                    None => Group::Span(format!("{name} {}", parts.context)),
                });
            }
            (None, false) => {
                parts.function = function(&own);
                parts.context = labels_context(record, &own);
                parts.group = Some(match own.get(REQUEST_LABEL.0) {
                    Some(request) => Group::Request(plain_value(request)),
                    None => Group::Function(parts.function.clone()),
                });
            }
        }
        if closed {
            if root.is_some() {
                let fields = labels_context(record, &span_own(&own));
                parts.nested = Some((escaped(record.message()), fields));
            }
            parts.message = close_message(&own);
            return parts;
        }
        let nearest_fields = nearest.as_ref().map(span_fields);
        if let (Some(nearest), Some(fields)) = (&nearest, &nearest_fields) {
            let name = nearest.get("name").and_then(Value::as_str).unwrap_or("");
            parts.nested = Some((escaped(name), context_pairs(fields)));
        }
        parts.message = lifecycle_mark(&own).map_or_else(
            || escaped(record.message()),
            |mark| format!("{mark} {}", escaped(record.message())),
        );
        if let Some(root) = &root {
            let root_fields = span_fields(root);
            let mut pairs = String::new();
            for (key, label) in [
                ("component", record.component()),
                ("operation", record.operation()),
            ] {
                let carried = nearest_fields
                    .as_ref()
                    .and_then(|fields| fields.get(key))
                    .or_else(|| root_fields.get(key))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !label.is_empty() && label != carried {
                    push_separated(&mut pairs, &pair(key, &Value::from(label)));
                }
            }
            push_separated(&mut pairs, &own_pairs(&own));
            push_separated(&mut parts.message, &pairs);
        }
        parts
    }

    /// The line, its columns padded to `widths`, the level in `color`.
    fn line(&self, widths: &Widths, color: LevelColor) -> String {
        let mut line = format!("{} ", self.timestamp);
        match self.color.filter(|_| color == LevelColor::Ansi) {
            Some(code) => {
                let _ = write!(line, "\x1b[{code}m{:<5}\x1b[0m", self.level);
            }
            None => {
                let _ = write!(line, "{:<5}", self.level);
            }
        }
        let _ = write!(
            line,
            " {:<width$}{FUNCTION_END}",
            self.function,
            width = widths.function
        );
        if !self.context.is_empty() {
            let _ = write!(
                line,
                "{:<width$}{CONTEXT_END}",
                self.context,
                width = widths.context
            );
        }
        if let Some((name, fields)) = &self.nested {
            let _ = write!(
                line,
                "{NESTED_MARK} {name:<width$} ",
                width = widths.nested_name
            );
            if !fields.is_empty() {
                let _ = write!(line, "{fields:<width$} ", width = widths.nested_fields);
            }
        }
        line.push_str(&self.message);
        let end = line.trim_end_matches(' ').len();
        line.truncate(end);
        line
    }
}

/// `fields` parsed as a JSON object; `None` for text that is not one.
fn parsed_fields(fields: &str) -> Option<Map<String, Value>> {
    if fields.is_empty() {
        return Some(Map::new());
    }
    serde_json::from_str::<Map<String, Value>>(fields).ok()
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

/// The `fields` object of one span member.
fn span_fields(span: &Map<String, Value>) -> Map<String, Value> {
    match span.get("fields") {
        Some(Value::Object(fields)) => fields.clone(),
        _ => Map::new(),
    }
}

/// The group of the records inside the root span `root`.
fn span_group(root: &Map<String, Value>, fields: &Map<String, Value>) -> Group {
    if let Some(request) = fields.get(REQUEST_LABEL.0) {
        return Group::Request(plain_value(request));
    }
    let name = root.get("name").and_then(Value::as_str).unwrap_or("");
    Group::Span(format!("{} {}", escaped(name), context_pairs(fields)))
}

/// A span close record's own fields: the span's fields without the members the close
/// adds.
fn span_own(own: &Map<String, Value>) -> Map<String, Value> {
    own.iter()
        .filter(|(key, _)| !CLOSE_MEMBERS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The context of a record outside every span, or of a span's own close: the record's
/// `component` and `operation`, then `fields` as [`context_pairs`] prints them.
fn labels_context(record: &LogRecord, fields: &Map<String, Value>) -> String {
    let mut labeled = fields.clone();
    for (key, value) in [
        ("component", record.component()),
        ("operation", record.operation()),
    ] {
        if !value.is_empty() {
            labeled.insert(key.to_owned(), Value::from(value));
        }
    }
    context_pairs(&labeled)
}

/// Span fields as `key=value` pairs: `component`, `operation`, then the rest sorted by
/// key, `request_id` under the label `req`, `code.function.name` left out.
fn context_pairs(fields: &Map<String, Value>) -> String {
    let mut pairs = String::new();
    for key in ["component", "operation"] {
        if let Some(value) = fields.get(key) {
            push_separated(&mut pairs, &pair(key, value));
        }
    }
    let mut rest = fields
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "component" | "operation" | FUNCTION_FIELD))
        .collect::<Vec<_>>();
    rest.sort_by_key(|(key, _)| *key);
    for (key, value) in rest {
        let key = if key == REQUEST_LABEL.0 {
            REQUEST_LABEL.1
        } else {
            key
        };
        push_separated(&mut pairs, &pair(key, value));
    }
    pairs
}

/// A record's own fields as `key=value` pairs sorted by key, `code.function.name` left
/// out.
fn own_pairs(fields: &Map<String, Value>) -> String {
    let mut sorted = fields
        .iter()
        .filter(|(key, _)| key.as_str() != FUNCTION_FIELD)
        .collect::<Vec<_>>();
    sorted.sort_by_key(|(key, _)| *key);
    let mut pairs = String::new();
    for (key, value) in sorted {
        push_separated(&mut pairs, &pair(key, value));
    }
    pairs
}

/// The message of a span close: `close`, the mark, the reason the operation did not
/// complete, then `busy` and `idle`.
///
/// The close is `✗` when its `status.code` is `Error`, it carries an `error.type`, or the
/// span's `outcome` is not a completion, so a record stored before its `status.code`
/// stated a recorded failure prints `✗` too. The reason is `panicked`, `cancelled`, or
/// `error.type=…`; a failure stated by `outcome` alone prints no reason, because the
/// span's fields before the message already print the `outcome`.
///
/// A close record written without `busy_ns` and `idle_ns` prints its `elapsed_ms`, and
/// one without `status.code`, `error.type`, or `outcome` prints no mark.
fn close_message(own: &Map<String, Value>) -> String {
    let text = |key: &str| own.get(key).map(plain_value);
    let error_type = text("error.type");
    let failed = text("status.code").as_deref() == Some("Error")
        || error_type.is_some()
        || text(OUTCOME_FIELD).is_some_and(|outcome| !completed_outcome(&outcome));
    let mut message = String::from("close");
    if failed {
        message.push(' ');
        message.push_str(FAILED_MARK);
        match error_type.as_deref() {
            Some("panic") => message.push_str(" panicked"),
            Some("cancelled") => message.push_str(" cancelled"),
            Some(other) => {
                message.push(' ');
                message.push_str(&pair("error.type", &Value::from(other)));
            }
            None => {}
        }
    } else if text("status.code").is_some() || own.contains_key(OUTCOME_FIELD) {
        message.push(' ');
        message.push_str(COMPLETED_MARK);
    }
    let nanoseconds = |key: &str| text(key).and_then(|value| value.parse::<u64>().ok());
    match (nanoseconds("busy_ns"), nanoseconds("idle_ns")) {
        (Some(busy), Some(idle)) => {
            let _ = write!(
                message,
                " busy={} idle={}",
                rendered_duration(busy),
                rendered_duration(idle)
            );
        }
        _ => {
            if let Some(elapsed) = own.get("elapsed_ms") {
                message.push(' ');
                message.push_str(&pair("elapsed_ms", elapsed));
            }
        }
    }
    message
}

/// The mark of a lifecycle record, from the fields that state its transition:
/// `phase = "start"` starts a phase, an `outcome` that is a completion (`ok`) completes
/// it, and any other `outcome`, such as `error` or `timeout`, fails it. `None` for any
/// other record.
fn lifecycle_mark(own: &Map<String, Value>) -> Option<&'static str> {
    let text = |key: &str| own.get(key).and_then(Value::as_str);
    match (text("phase"), text(OUTCOME_FIELD)) {
        (Some("start"), _) => Some(STARTED_MARK),
        (_, Some(outcome)) if completed_outcome(outcome) => Some(COMPLETED_MARK),
        (_, Some(_)) => Some(FAILED_MARK),
        _ => None,
    }
}

/// A duration in nanoseconds with three significant digits and the unit `ns`, `µs`,
/// `ms`, or `s`.
///
/// The rule of `tracing-subscriber`'s `TimingDisplay` (`fmt/format/mod.rs`), which that
/// crate declares `pub(super)`: two decimals below 10, one below 100, none below 1,000,
/// then the next unit; seconds past 1,000 print whole.
fn rendered_duration(nanoseconds: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a duration past 2^53 nanoseconds, 104 days, prints three digits"
    )]
    let mut value = nanoseconds as f64;
    for unit in ["ns", "µs", "ms", "s"] {
        if value < 10.0 {
            return format!("{value:.2}{unit}");
        } else if value < 100.0 {
            return format!("{value:.1}{unit}");
        } else if value < 1_000.0 {
            return format!("{value:.0}{unit}");
        }
        value /= 1_000.0;
    }
    format!("{:.0}s", value * 1_000.0)
}

/// `key=value`, both escaped, a string value without the quotes JSON puts around it.
///
/// An array or object value prints as the JSON `serde_json` writes, which escapes C0
/// controls alone, so its text is escaped as well.
fn pair(key: &str, value: &Value) -> String {
    let mut text = escaped(key);
    text.push('=');
    match value {
        Value::String(value) => push_escaped(&mut text, value),
        other => push_escaped(&mut text, &other.to_string()),
    }
    text
}

/// A field value as text: a string without its quotes, any other value as JSON.
fn plain_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Appends `text` to `line`, after one space when `line` holds something already.
fn push_separated(line: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !line.is_empty() {
        line.push(' ');
    }
    line.push_str(text);
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

/// Appends `text` to `line` with every character [`is_escaped`] names written as
/// [`char::escape_debug`] writes it (`\u{1b}`, `\n`, `\u{202e}`), every other character
/// as it is.
///
/// Every piece of record text a line carries passes through here: the message, the
/// level, `component`, `operation`, the function, span names, field keys and values, and
/// the JSON of an array or object value. A record's text comes partly from outside the
/// process (file paths, tool arguments, text of an indexed repository), and a line reaches
/// a terminal on stderr and through `rift server logs`, so no record can move the cursor,
/// recolor the terminal, end its line early, or reorder the text a reader sees. The only
/// escape sequences a line carries are the level colors [`LevelColor::Ansi`] writes.
///
/// A backslash prints as it is: a Windows path stays readable, and the text `\u{1b}` a
/// record carried reads the same as an escaped ESC without acting as one.
fn push_escaped(line: &mut String, text: &str) {
    if !text.chars().any(is_escaped) {
        line.push_str(text);
        return;
    }
    for character in text.chars() {
        if is_escaped(character) {
            let _ = write!(line, "{}", character.escape_debug());
        } else {
            line.push(character);
        }
    }
}

/// `text` with every character [`is_escaped`] names in its escaped form.
fn escaped(text: &str) -> String {
    let mut line = String::with_capacity(text.len());
    push_escaped(&mut line, text);
    line
}

/// Whether a line prints `character` escaped:
///
/// - a control code, `char::is_control`: C0 (`\0` to `\x1f`, with ESC, CR, LF, and TAB),
///   DEL (`\x7f`), and C1 (`\u{80}` to `\u{9f}`). ESC and the C1 CSI start terminal
///   control sequences; CR and LF end the line; the line layout uses no TAB.
/// - U+2028 and U+2029, the line and paragraph separators: Python's `str.splitlines`, with
///   which the `rift-dev` log readers split a page into lines, ends a line at them.
/// - The bidirectional embeddings, overrides, and isolates U+202A to U+202E and U+2066 to
///   U+2069: a terminal that applies the Unicode bidirectional algorithm displays the text
///   after one in an order other than the order of its bytes, so a value can show a reader
///   text it does not hold.
pub(crate) const fn is_escaped(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// One recorded instant in UTC with milliseconds, `2026-10-04 20:42:58.787Z`. A
/// millisecond count outside jiff's representable range prints the raw count.
fn rendered_timestamp(recorded_at_ms: i64) -> String {
    Timestamp::from_millisecond(recorded_at_ms).map_or_else(
        |_| recorded_at_ms.to_string(),
        |timestamp| timestamp.strftime(TIMESTAMP_FORMAT).to_string(),
    )
}

#[cfg(test)]
mod tests;
