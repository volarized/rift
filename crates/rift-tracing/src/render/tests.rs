use serde_json::{Value, json};

use super::{LevelColor, LogLines, is_escaped, rendered_duration, rendered_timestamp};
use crate::record::LogRecord;

/// 2026-10-04 20:42:58.787Z, the first instant of the owner's sample lines.
const SAMPLE_MS: i64 = 1_791_146_578_787;
/// The function that opens the request spans of the sample.
const NODES: &str = "rift_mcp::server::Server::nodes";

/// A log record at `SAMPLE_MS + offset_ms` with `fields`.
fn record(
    offset_ms: i64,
    level: &str,
    labels: (&str, &str),
    message: &str,
    fields: &Value,
) -> LogRecord {
    LogRecord::new(
        SAMPLE_MS + offset_ms,
        level,
        "rift_mcp::server",
        labels.0,
        labels.1,
        message,
        &fields.to_string(),
    )
}

/// The `root_span` member of a `tools/call` request `request` for `tool`.
fn request_span(request: u32, tool: &str) -> Value {
    json!({
        "name": "mcp.request",
        "fields": {
            "component": "mcp",
            "operation": "tools/call",
            "code.function.name": NODES,
            "request_id": request.to_string(),
            "tool": tool,
        },
    })
}

/// The close record of a span below the request `request`.
fn nested_close(offset_ms: i64, name: &str, busy_ns: u64, idle_ns: u64, request: u32) -> LogRecord {
    record(
        offset_ms,
        "info",
        ("index", name),
        name,
        &json!({
            "code.function.name": "rift_index::workspace::Workspace::fingerprint",
            "span": "closed",
            "elapsed_ms": "0",
            "busy_ns": busy_ns.to_string(),
            "idle_ns": idle_ns.to_string(),
            "status.code": "Ok",
            "root_span": request_span(request, "nodes"),
        }),
    )
}

/// The close record of the request span `request` itself.
fn request_close(offset_ms: i64, request: u32, tool: &str, status: &Value) -> LogRecord {
    let mut fields = json!({
        "code.function.name": NODES,
        "request_id": request.to_string(),
        "tool": tool,
        "span": "closed",
        "elapsed_ms": "6",
        "busy_ns": "6550000",
        "idle_ns": "290000",
    });
    if let (Value::Object(fields), Value::Object(status)) = (&mut fields, status) {
        fields.extend(status.clone());
    }
    record(
        offset_ms,
        "info",
        ("mcp", "tools/call"),
        "mcp.request",
        &fields,
    )
}

/// The lines a stored page prints for `records`.
fn page(records: &[LogRecord]) -> String {
    LogLines::stored_page().lines(records)
}

/// The owner's second sample group: two operations nested in a `tools/call` request, then
/// the request's own close. Each column is padded to the widest value of the group, and
/// the nested operation column appears on the lines that have one.
#[test]
fn a_stored_page_prints_a_request_group_in_the_settled_layout() {
    let records = [
        nested_close(11, "fingerprint.configuration", 96_900, 13_400, 11),
        nested_close(12, "fingerprint.fold", 12_100, 13_200, 11),
        request_close(12, 11, "nodes", &json!({"status.code": "Ok"})),
    ];

    assert_eq!(
        page(&records),
        "2026-10-04 20:42:58.798Z INFO  rift_mcp::server::Server::nodes   component=mcp \
         operation=tools/call req=11 tool=nodes  ↳ fingerprint.configuration component=index \
         operation=fingerprint.configuration close ✓ busy=96.9µs idle=13.4µs\n\
         2026-10-04 20:42:58.799Z INFO  rift_mcp::server::Server::nodes   component=mcp \
         operation=tools/call req=11 tool=nodes  ↳ fingerprint.fold          component=index \
         operation=fingerprint.fold          close ✓ busy=12.1µs idle=13.2µs\n\
         2026-10-04 20:42:58.799Z INFO  rift_mcp::server::Server::nodes   component=mcp \
         operation=tools/call req=11 tool=nodes  close ✓ busy=6.55ms idle=290µs\n"
    );
}

/// An event outside every span prints the function that emitted it, its own labels and
/// fields as the context, then its message.
#[test]
fn an_event_outside_every_span_prints_its_fields_as_the_context() {
    let event = record(
        0,
        "info",
        ("mcp", ""),
        "MCP server starting",
        &json!({"code.function.name": "rift_mcp::http::serve", "transport": "http"}),
    );

    assert_eq!(
        page(&[event]),
        "2026-10-04 20:42:58.787Z INFO  rift_mcp::http::serve   component=mcp transport=http  \
         MCP server starting\n"
    );
}

/// An event inside a request prints the request's context, the nearest span after `↳`,
/// the message, and its own fields, with a label the nearest span does not carry.
#[test]
fn an_event_in_nested_spans_prints_the_root_the_nearest_span_and_its_own_fields() {
    let event = record(
        0,
        "debug",
        ("search", "fingerprint.discover"),
        "walked the workspace",
        &json!({
            "code.function.name": "rift_index::workspace::walk",
            "files": "12",
            "root_span": request_span(18, "get_symbol"),
            "nearest_span": {
                "name": "fingerprint.discover",
                "fields": {"component": "index", "operation": "fingerprint.discover"},
            },
        }),
    );

    assert_eq!(
        page(&[event]),
        "2026-10-04 20:42:58.787Z DEBUG rift_mcp::server::Server::nodes   component=mcp \
         operation=tools/call req=18 tool=get_symbol  ↳ fingerprint.discover component=index \
         operation=fingerprint.discover walked the workspace component=search files=12\n"
    );
}

/// A close of an operation that did not complete prints `✗` and its reason before the
/// durations: `error.type`, `panicked`, or `cancelled`.
#[test]
fn a_close_that_did_not_complete_prints_its_reason() {
    let line = |status: Value| {
        let line = page(&[request_close(0, 3, "search", &status)]);
        line.split_once("tool=search  ")
            .map(|(_, message)| message.to_owned())
            .unwrap_or(line)
    };

    assert_eq!(
        line(json!({"status.code": "Error", "error.type": "index.lexical_storage"})),
        "close ✗ error.type=index.lexical_storage busy=6.55ms idle=290µs\n"
    );
    assert_eq!(
        line(json!({"status.code": "Error", "error.type": "panic"})),
        "close ✗ panicked busy=6.55ms idle=290µs\n"
    );
    assert_eq!(
        line(json!({"status.code": "Error", "error.type": "cancelled"})),
        "close ✗ cancelled busy=6.55ms idle=290µs\n"
    );
    assert_eq!(
        line(json!({})),
        "close busy=6.55ms idle=290µs\n",
        "a close record without `status.code` states no outcome"
    );
    assert_eq!(
        line(json!({"status.code": "Error", "outcome": "error"})),
        "close ✗ busy=6.55ms idle=290µs\n",
        "a failure stated by `outcome` alone prints no reason"
    );
    assert_eq!(
        line(json!({"status.code": "Ok", "outcome": "error"})),
        "close ✗ busy=6.55ms idle=290µs\n",
        "a record stored before its `status.code` stated the `outcome` still prints `✗`"
    );
    assert_eq!(
        line(json!({"outcome": "timeout"})),
        "close ✗ busy=6.55ms idle=290µs\n"
    );
    assert_eq!(
        line(json!({"status.code": "Ok", "error.type": "timeout"})),
        "close ✗ error.type=timeout busy=6.55ms idle=290µs\n"
    );
    assert_eq!(
        line(json!({"status.code": "Ok", "outcome": "ok"})),
        "close ✓ busy=6.55ms idle=290µs\n"
    );
    assert_eq!(
        line(json!({"outcome": "acquired"})),
        "close ✓ busy=6.55ms idle=290µs\n",
        "a lock wait that acquired its lock completed"
    );
}

/// A lifecycle record prints the mark its `phase` or `outcome` states before its message,
/// and keeps that field among its own.
#[test]
fn a_lifecycle_record_prints_its_mark() {
    let lifecycle = |state: Value, message: &str| {
        let mut fields = json!({"code.function.name": "rift_mcp::validation::prepare"});
        if let (Value::Object(fields), Value::Object(state)) = (&mut fields, state) {
            fields.extend(state);
        }
        record(0, "info", ("index", "index.build"), message, &fields)
    };

    assert_eq!(
        page(&[lifecycle(
            json!({"phase": "start", "total": "3"}),
            "index preparation started"
        )]),
        "2026-10-04 20:42:58.787Z INFO  rift_mcp::validation::prepare   component=index \
         operation=index.build phase=start total=3  → index preparation started\n"
    );
    assert_eq!(
        page(&[lifecycle(
            json!({"outcome": "ok"}),
            "lexical commit settled"
        )]),
        "2026-10-04 20:42:58.787Z INFO  rift_mcp::validation::prepare   component=index \
         operation=index.build outcome=ok  ✓ lexical commit settled\n"
    );
    assert_eq!(
        page(&[lifecycle(
            json!({"outcome": "error", "error": "refused"}),
            "the commit failed"
        )]),
        "2026-10-04 20:42:58.787Z INFO  rift_mcp::validation::prepare   component=index \
         operation=index.build error=refused outcome=error  ✗ the commit failed\n"
    );
    assert_eq!(
        page(&[lifecycle(
            json!({"outcome": "timeout", "stage": "log drain"}),
            "stop stage ended"
        )]),
        "2026-10-04 20:42:58.787Z INFO  rift_mcp::validation::prepare   component=index \
         operation=index.build outcome=timeout stage=log drain  ✗ stop stage ended\n",
        "a phase that ran out its deadline did not complete"
    );
}

/// A lifecycle record inside a span prints its mark and message, then its own fields.
#[test]
fn a_lifecycle_record_inside_a_span_prints_its_fields_after_the_message() {
    let event = record(
        0,
        "info",
        ("mcp", "tools/call"),
        "history fill started",
        &json!({"phase": "start", "pending": "2", "root_span": request_span(4, "nodes")}),
    );

    let line = page(&[event]);
    assert!(
        line.ends_with("tool=nodes  → history fill started pending=2 phase=start\n"),
        "{line}"
    );
}

/// A metric snapshot record prints its instrument group in the function column and its
/// values as the message, and is a group of its own.
#[test]
fn a_metric_snapshot_prints_its_group_and_values() {
    let snapshot = LogRecord::new(
        SAMPLE_MS,
        "info",
        "rift_tracing::metric",
        "",
        "runtime",
        "metric snapshot",
        "{\"tokio.runtime.task.count\":42,\"tokio.runtime.global_queue.length\":7}",
    )
    .into_metric();

    assert_eq!(
        page(&[snapshot.clone(), snapshot]),
        "2026-10-04 20:42:58.787Z INFO  runtime   tokio.runtime.global_queue.length=7 \
         tokio.runtime.task.count=42\n\n\
         2026-10-04 20:42:58.787Z INFO  runtime   tokio.runtime.global_queue.length=7 \
         tokio.runtime.task.count=42\n"
    );
}

/// A blank line separates two groups: two requests, then a record outside every span.
/// Each group pads to its own widths, and a later call breaks against the group the
/// previous call ended in.
#[test]
fn groups_are_separated_by_a_blank_line_and_padded_apart() {
    let started = record(
        20,
        "info",
        ("mcp", ""),
        "MCP proxy starting",
        &json!({"code.function.name": "rift_mcp::proxy::serve", "transport": "stdio"}),
    );
    let mut lines = LogLines::stored_page();
    let first = lines.lines(&[
        nested_close(0, "fingerprint.fold", 12_100, 13_200, 10),
        request_close(1, 10, "nodes", &json!({"status.code": "Ok"})),
        nested_close(2, "fingerprint.fold", 12_100, 13_200, 11),
    ]);
    let second = lines.lines(&[
        request_close(3, 11, "nodes", &json!({"status.code": "Ok"})),
        started,
    ]);

    let blank = |text: &str| text.lines().filter(|line| line.is_empty()).count();
    assert_eq!(blank(&first), 1, "{first}");
    assert!(first.ends_with("\n\n2026-10-04 20:42:58.789Z INFO  rift_mcp::server::Server::nodes   component=mcp operation=tools/call req=11 tool=nodes  ↳ fingerprint.fold component=index operation=fingerprint.fold close ✓ busy=12.1µs idle=13.2µs\n"), "{first}");
    assert!(!second.starts_with('\n'), "request 11 continues: {second}");
    assert_eq!(blank(&second), 1, "{second}");
    assert!(
        second.ends_with(
            "\n\n2026-10-04 20:42:58.807Z INFO  rift_mcp::proxy::serve   \
                          component=mcp transport=stdio  MCP proxy starting\n"
        ),
        "{second}"
    );
}

/// A live stream pads every column to its fixed width, and a wider value pushes the rest
/// of its own line.
#[test]
fn a_live_stream_pads_to_fixed_widths() {
    let event = record(
        0,
        "warn",
        ("index", "index.supervisor"),
        "the index supervisor stopped",
        &json!({"code.function.name": "rift_mcp::validation::supervise", "reason": "shutdown"}),
    );
    let wide = record(
        1,
        "warn",
        ("index", ""),
        "late",
        &json!({"code.function.name": "rift_mcp::validation::a_function_name_past_the_column"}),
    );

    let mut lines = LogLines::live_stream();
    let text = lines.lines(&[event.clone(), wide]);

    assert_eq!(
        text,
        format!(
            "2026-10-04 20:42:58.787Z WARN  {:<36}   {:<48}  the index supervisor stopped\n\n\
             2026-10-04 20:42:58.788Z WARN  {:<36}   {:<48}  late\n",
            "rift_mcp::validation::supervise",
            "component=index operation=index.supervisor reason=shutdown",
            "rift_mcp::validation::a_function_name_past_the_column",
            "component=index",
        )
    );
    assert_eq!(
        format!("{}\n", event.rendered()),
        text.lines()
            .next()
            .map(|line| format!("{line}\n"))
            .unwrap_or_default()
    );
}

/// A record that carries no function prints its target there.
#[test]
fn a_record_without_a_function_prints_its_target() {
    let event = LogRecord::new(SAMPLE_MS, "info", "toasty::query", "", "", "ran", "{}");

    assert_eq!(
        page(&[event]),
        "2026-10-04 20:42:58.787Z INFO  toasty::query   ran\n"
    );
}

/// Fields the store holds as text that is not a JSON object print after the message.
#[test]
fn fields_that_are_not_an_object_print_as_text() {
    let event = LogRecord::new(SAMPLE_MS, "info", "rift", "mcp", "", "odd", "not json");

    assert_eq!(
        page(&[event]),
        "2026-10-04 20:42:58.787Z INFO  rift   component=mcp  odd not json\n"
    );
}

/// A duration prints three significant digits at the 10, 100, and 1,000 boundaries of
/// each unit, as `tracing-subscriber`'s `TimingDisplay` does.
#[test]
fn a_duration_prints_three_significant_digits() {
    for (nanoseconds, printed) in [
        (0, "0.00ns"),
        (9, "9.00ns"),
        (10, "10.0ns"),
        (99, "99.0ns"),
        (100, "100ns"),
        (999, "999ns"),
        (1_000, "1.00µs"),
        (96_900, "96.9µs"),
        (100_000, "100µs"),
        (999_000, "999µs"),
        (2_390_000, "2.39ms"),
        (13_400_000, "13.4ms"),
        (290_000_000, "290ms"),
        (1_000_000_000, "1.00s"),
        (10_000_000_000, "10.0s"),
        (100_000_000_000, "100s"),
        (1_234_000_000_000, "1234s"),
    ] {
        assert_eq!(rendered_duration(nanoseconds), printed, "{nanoseconds}");
    }
}

#[test]
fn a_timestamp_prints_utc_with_milliseconds_and_z() {
    assert_eq!(rendered_timestamp(SAMPLE_MS), "2026-10-04 20:42:58.787Z");
    assert_eq!(rendered_timestamp(0), "1970-01-01 00:00:00.000Z");
    assert_eq!(rendered_timestamp(-1), "1969-12-31 23:59:59.999Z");
    assert_eq!(rendered_timestamp(i64::MAX), i64::MAX.to_string());
}

/// Text a record can carry from outside the process, each holding characters a terminal
/// or a line reader acts on, and the escaped form a line prints for one of them.
const HOSTILE: [(&str, &str); 11] = [
    ("\u{1b}[31mred\u{1b}[0m", "\\u{1b}[31mred"),
    ("\u{1b}]0;title\u{7}", "\\u{1b}]0;title\\u{7}"),
    ("\u{1b}[2J\u{1b}[H", "\\u{1b}[2J"),
    ("left\rright", "left\\rright"),
    ("first\nsecond", "first\\nsecond"),
    ("nul\0byte", "nul\\0byte"),
    ("del\u{7f}byte", "del\\u{7f}byte"),
    ("csi\u{9b}31m", "csi\\u{9b}31m"),
    ("next\u{85}line", "next\\u{85}line"),
    ("override\u{202e}txt.exe", "override\\u{202e}txt.exe"),
    (
        "isolate\u{2066}x\u{2069} sep\u{2028}end",
        "isolate\\u{2066}x\\u{2069}",
    ),
];

/// Records carrying `text` in each place a line prints record text, each with whether the
/// line shows that text: the message, the labels, a field key and value, an array value,
/// the function, the root span's field key and value, the nearest span's name and field,
/// a close record's span field, and fields that are not JSON. The root span's name does
/// not print.
fn records_carrying(text: &str) -> Vec<(LogRecord, bool)> {
    let span = |name: &str, key: &str, value: &str| {
        let fields = json!({ "component": "mcp", key: value });
        json!({ "name": name, "fields": fields })
    };
    let with_fields = |fields: &Value| record(0, "info", ("mcp", "tools/call"), "plain", fields);
    vec![
        (
            record(0, "info", ("mcp", "tools/call"), text, &json!({})),
            true,
        ),
        (record(0, "info", (text, text), "plain", &json!({})), true),
        (with_fields(&json!({ "path": text })), true),
        (with_fields(&json!({ text: "value" })), true),
        (
            with_fields(&json!({ "arguments": [text, { "nested": text }] })),
            false,
        ),
        (with_fields(&json!({ "code.function.name": text })), true),
        (
            with_fields(&json!({ "root_span": span(text, "tool", "search") })),
            false,
        ),
        (
            with_fields(&json!({ "root_span": span("mcp.request", "tool", text) })),
            true,
        ),
        (
            with_fields(&json!({ "root_span": span("mcp.request", text, "search") })),
            true,
        ),
        (
            with_fields(&json!({
                "root_span": span("mcp.request", "tool", "search"),
                "nearest_span": span(text, "path", "lib.rs"),
            })),
            true,
        ),
        (
            with_fields(&json!({
                "root_span": span("mcp.request", "tool", "search"),
                "nearest_span": span("index.read", "path", text),
            })),
            true,
        ),
        (
            record(
                0,
                "info",
                ("index", "index.read"),
                text,
                &json!({
                    "path": text,
                    "span": "closed",
                    "status.code": "Error",
                    "error.type": text,
                    "root_span": span("mcp.request", "tool", "search"),
                }),
            ),
            true,
        ),
        (
            LogRecord::new(0, "info", "rift", "mcp", "", "plain", text),
            true,
        ),
    ]
}

/// `line` without the level colors [`LevelColor::Ansi`] writes.
fn without_level_colors(line: &str) -> String {
    let mut plain = line.to_owned();
    for code in [31, 32, 33, 34, 35, 0] {
        plain = plain.replace(&format!("\u{1b}[{code}m"), "");
    }
    plain
}

/// No record text reaches a line as a control character, a line separator, or a
/// bidirectional control: each prints escaped, and the record stays one line, colored or
/// not, on a live stream and on a stored page.
#[test]
fn record_text_never_reaches_a_line_as_a_control_character() {
    for (text, escaped_form) in HOSTILE {
        for (record, shown) in records_carrying(text) {
            let page = page(std::slice::from_ref(&record));
            for line in [
                record.rendered_line(LevelColor::Plain),
                record.rendered_line(LevelColor::Ansi),
                page.trim_end_matches('\n').to_owned(),
            ] {
                let plain = without_level_colors(&line);
                let raw = plain.chars().filter(|character| is_escaped(*character));
                assert_eq!(raw.count(), 0, "{text:?} reached {line:?}");
                assert_eq!(line.lines().count(), 1, "{line:?}");
                assert!(!line.contains(['\n', '\r']), "{line:?}");
            }
            if shown {
                assert!(
                    page.contains(escaped_form),
                    "{page:?} shows {escaped_form:?}"
                );
            }
        }
    }
}

/// The only escape sequences a line carries are its level colors, written on a terminal
/// alone.
#[test]
fn a_plain_line_carries_no_escape_sequence_and_a_terminal_line_only_its_color() {
    let record = LogRecord::new(0, "warn", "rift", "mcp", "", "\u{1b}[31mred", "{}");

    let plain = record.rendered_line(LevelColor::Plain);
    let terminal = record.rendered_line(LevelColor::Ansi);

    assert!(!plain.contains('\u{1b}'), "{plain:?}");
    assert_eq!(terminal.matches('\u{1b}').count(), 2, "{terminal:?}");
    assert!(
        terminal.contains("\u{1b}[33mWARN \u{1b}[0m"),
        "{terminal:?}"
    );
    assert!(terminal.ends_with("\\u{1b}[31mred"), "{terminal:?}");
}
