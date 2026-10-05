//! The layer that prints records on standard error, and standard error of a detached
//! server, cut at a byte bound.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use jiff::tz::TimeZone;
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::capture::{closed_record, event_record, span_opened, span_recorded};
use crate::record::LogRecord;
use crate::render::LevelColor;

/// The `tracing` layer that prints each admitted event and span close as one line on its
/// writer, in the form [`LogRecord::rendered`] gives `rift server logs`, with the time in
/// UTC.
///
/// It builds the record the log capture builds, from labels it keeps per span itself, and
/// writes the rendered line in one `write_all` call. No module path and no list of every
/// enclosing span precede the line; a span close prints its record, the span's name with
/// `elapsed_ms`.
pub(crate) struct StderrLines<W> {
    writer: W,
    color: LevelColor,
}

impl<W> StderrLines<W> {
    /// Lines on `writer`, the level in `color`.
    pub(crate) const fn new(writer: W, color: LevelColor) -> Self {
        Self { writer, color }
    }
}

impl<W> StderrLines<W>
where
    W: for<'writer> MakeWriter<'writer> + 'static,
{
    /// Writes `record` as one line. A failed write is dropped: stderr has no reader to
    /// report it to.
    fn print(&self, record: &LogRecord) {
        let mut line = record.rendered_line(&TimeZone::UTC, self.color);
        line.push('\n');
        let _ = self.writer.make_writer().write_all(line.as_bytes());
    }
}

impl<S, W> Layer<S> for StderrLines<W>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    W: for<'writer> MakeWriter<'writer> + 'static,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        span_opened::<Self, S>(attributes, id, &context);
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        span_recorded::<Self, S>(id, values, &context);
    }

    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        if let Some(record) = closed_record::<Self, S>(&id, &context) {
            self.print(&record);
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        self.print(&event_record::<Self, S>(event, &context));
    }
}

/// Bytes of traced diagnostics a server writes to its standard error before
/// it stops writing there, when that stream is not a terminal.
///
/// The file `rift server start` hands its server is what a crashed server
/// leaves behind: it holds the start, and a panic's own report reaches it
/// through the default panic hook past this bound. The diagnostics of a long
/// life go to `rift server logs`.
pub const SERVER_STDERR_BYTES_MAX: u64 = 1 << 20;
/// The line the writer prints once, as the last thing, when the bound is reached.
const SERVER_STDERR_BOUND_NOTICE: &str =
    "rift: standard error reached its byte bound; later diagnostics are under `rift server logs`\n";

/// Standard error of a server whose stream is a file, cut at
/// [`SERVER_STDERR_BYTES_MAX`].
///
/// The file `rift server start` hands its server would otherwise grow for
/// the server's whole life. Past the bound the writer prints one notice and
/// drops what it is handed afterwards; the diagnostics recorded under
/// `rift server logs` are unaffected.
#[derive(Debug, Default)]
pub(crate) struct BoundedStderr {
    written: AtomicU64,
}

impl<'a> MakeWriter<'a> for BoundedStderr {
    type Writer = BoundedWriter<'a, io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        BoundedWriter::new(&self.written, io::stderr())
    }
}

/// One writer over a shared byte count: writes pass through until the count
/// reaches [`SERVER_STDERR_BYTES_MAX`], the crossing write is followed by
/// the notice, and later writes are counted and dropped.
#[derive(Debug)]
pub(crate) struct BoundedWriter<'a, Sink: Write> {
    written: &'a AtomicU64,
    sink: Sink,
}

impl<'a, Sink: Write> BoundedWriter<'a, Sink> {
    /// A writer over `sink` sharing `written` with every sibling writer.
    pub(crate) const fn new(written: &'a AtomicU64, sink: Sink) -> Self {
        Self { written, sink }
    }
}

impl<Sink: Write> Write for BoundedWriter<'_, Sink> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let before = self
            .written
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if before >= SERVER_STDERR_BYTES_MAX {
            return Ok(bytes.len());
        }
        self.sink.write_all(bytes)?;
        if before + bytes.len() as u64 >= SERVER_STDERR_BYTES_MAX {
            self.sink.write_all(SERVER_STDERR_BOUND_NOTICE.as_bytes())?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use jiff::tz::TimeZone;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{BoundedWriter, SERVER_STDERR_BOUND_NOTICE, SERVER_STDERR_BYTES_MAX, StderrLines};
    use crate::render::LevelColor;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// The bytes a [`StderrLines`] layer wrote, shared with the test that reads them.
    #[derive(Clone, Default)]
    struct Written(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Written {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the written bytes are not poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Written {
        /// Every printed line without its timestamp, `elapsed_ms` read as `_`.
        fn lines(&self) -> Vec<String> {
            let bytes = self.0.lock().expect("the written bytes are not poisoned");
            String::from_utf8_lossy(&bytes)
                .lines()
                .map(without_timestamp)
                .collect()
        }
    }

    /// `line` past its timestamp, which must be UTC, with the `elapsed_ms` value as `_`: the
    /// two vary between runs, and every other column is exact.
    fn without_timestamp(line: &str) -> String {
        let (timestamp, rest) = line
            .split_once(' ')
            .expect("a line starts with its timestamp");
        assert!(timestamp.ends_with("+00:00"), "{line}");
        let mut parts = rest.split(' ').map(str::to_owned).collect::<Vec<_>>();
        for part in &mut parts {
            if part.starts_with("elapsed_ms=") {
                *part = "elapsed_ms=_".to_owned();
            }
        }
        parts.join(" ")
    }

    /// Runs `emit` under a subscriber whose stderr lines go to the returned buffer.
    fn printed(emit: impl FnOnce()) -> Written {
        let written = Written::default();
        let writer = written.clone();
        let layer = StderrLines::new(move || writer.clone(), LevelColor::Plain);
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), emit);
        written
    }

    /// The owner's case: an index operation inside a `tools/call` request.
    fn request_with_index_operation() {
        let request = tracing::info_span!(
            "mcp.request",
            component = "mcp",
            operation = "tools/call",
            request_id = 18,
            tool = "get_symbol"
        );
        let _request = request.enter();
        let discover = tracing::info_span!(
            "fingerprint.discover",
            component = "index",
            operation = "fingerprint.discover"
        );
        discover.in_scope(|| tracing::info!(files = 12, "walked the workspace"));
    }

    /// None of the default format: no enclosing span in braces, no module path, no span timing.
    fn assert_no_default_format(lines: &[String]) {
        for line in lines {
            assert!(!line.contains('{'), "{line}");
            assert!(!line.contains("time.busy"), "{line}");
            assert!(!line.contains("rift_"), "{line}");
        }
    }

    #[test]
    fn an_event_outside_every_span_prints_its_labels_message_and_fields() {
        let lines = printed(|| {
            tracing::info!(component = "mcp", transport = "http", "MCP server starting");
        })
        .lines();

        assert_eq!(
            lines,
            ["INFO  mcp      -            MCP server starting transport=http"]
        );
        assert_no_default_format(&lines);
    }

    #[test]
    fn an_event_inside_two_spans_prints_the_request_and_the_nested_operation() {
        let lines = printed(request_with_index_operation).lines();

        assert_eq!(
            lines[0],
            "INFO  index    fingerprint.discover component=mcp operation=tools/call req=18 \
             tool=get_symbol ↳ fingerprint.discover  walked the workspace files=12"
        );
        assert_no_default_format(&lines);
    }

    #[test]
    fn a_span_close_prints_its_name_and_elapsed_time_under_the_request() {
        let lines = printed(request_with_index_operation).lines();

        assert_eq!(
            lines[1..],
            [
                "INFO  index    fingerprint.discover component=mcp operation=tools/call req=18 \
                 tool=get_symbol  fingerprint.discover elapsed_ms=_ span=closed",
                "INFO  mcp      tools/call   mcp.request elapsed_ms=_ request_id=18 \
                 span=closed tool=get_symbol",
            ]
        );
        assert_no_default_format(&lines);
    }

    #[test]
    fn a_warning_prints_its_level_and_fields() {
        let lines = printed(|| {
            tracing::warn!(
                component = "index",
                operation = "index.supervisor",
                published = 3,
                reason = "shutdown",
                "the index supervisor stopped"
            );
        })
        .lines();

        assert_eq!(
            lines,
            [
                "WARN  index    index.supervisor the index supervisor stopped published=3 \
                 reason=shutdown"
            ]
        );
        assert_no_default_format(&lines);
    }

    /// Stderr and `rift server logs` print one form: the stderr line of each record is the
    /// line the captured record renders to.
    #[test]
    fn a_stderr_line_is_the_rendered_line_of_the_captured_record() {
        let written = Written::default();
        let writer = written.clone();
        let (sink, mut drain) = crate::log_capture();
        let subscriber = tracing_subscriber::registry()
            .with(StderrLines::new(move || writer.clone(), LevelColor::Plain))
            .with(sink);
        tracing::subscriber::with_default(subscriber, || {
            request_with_index_operation();
            tracing::warn!(component = "index", reason = "shutdown", "stopped");
        });

        let rendered = std::iter::from_fn(|| drain.try_recv_record().ok())
            .map(|record| without_timestamp(&record.rendered(&TimeZone::UTC)))
            .collect::<Vec<_>>();
        assert_eq!(rendered.len(), 4);
        assert_eq!(written.lines(), rendered);
    }

    /// Text from outside the process reaches stderr escaped: one record, one line, and no
    /// control character a terminal acts on.
    #[test]
    fn a_stderr_line_escapes_control_characters_in_record_text() {
        let written = printed(|| {
            let request = tracing::info_span!(
                "mcp.request",
                component = "mcp",
                tool = "search\u{1b}[2J",
                request_id = 3
            );
            let _request = request.enter();
            let read = tracing::info_span!("index.read", path = "a\u{202e}b\nc");
            read.in_scope(|| {
                tracing::warn!(query = "q\r\u{9b}31m\u{7f}\0", "found\u{1b}]0;title\u{7}");
            });
        });

        let bytes = written
            .0
            .lock()
            .expect("the written bytes are not poisoned")
            .clone();
        let text = String::from_utf8(bytes).expect("a line is UTF-8");
        assert_eq!(text.matches('\n').count(), 3, "{text:?}");
        for line in text.lines() {
            assert!(
                !line.chars().any(super::super::render::is_escaped),
                "{line:?}"
            );
        }
        assert!(text.contains("search\\u{1b}[2J"), "{text:?}");
        assert!(text.contains("a\\u{202e}b\\nc"), "{text:?}");
        assert!(text.contains("q\\r\\u{9b}31m\\u{7f}\\0"), "{text:?}");
        assert!(text.contains("found\\u{1b}]0;title\\u{7}"), "{text:?}");
    }

    #[test]
    fn a_bounded_writer_passes_the_crossing_write_then_drops() -> TestResult {
        let written = AtomicU64::new(0);
        let mut sink = Vec::new();
        let head = vec![b'a'; usize::try_from(SERVER_STDERR_BYTES_MAX)? - 4];
        {
            let mut writer = BoundedWriter::new(&written, &mut sink);
            writer.write_all(&head)?;
            writer.write_all(b"crossing")?;
            writer.write_all(b"dropped")?;
            writer.flush()?;
        }
        let expected_length = head.len() + "crossing".len() + SERVER_STDERR_BOUND_NOTICE.len();
        assert_eq!(sink.len(), expected_length);
        assert!(sink.ends_with(SERVER_STDERR_BOUND_NOTICE.as_bytes()));
        assert!(!sink.windows(7).any(|window| window == b"dropped"));
        assert_eq!(
            written.load(Ordering::Relaxed),
            (head.len() + "crossing".len() + "dropped".len()) as u64,
            "dropped bytes are still counted"
        );
        Ok(())
    }

    #[test]
    fn a_bounded_writer_shares_its_count_between_writers() -> TestResult {
        let written = AtomicU64::new(SERVER_STDERR_BYTES_MAX);
        let mut sink = Vec::new();
        BoundedWriter::new(&written, &mut sink).write_all(b"late")?;
        assert!(sink.is_empty(), "a writer past the bound writes nothing");
        Ok(())
    }
}
