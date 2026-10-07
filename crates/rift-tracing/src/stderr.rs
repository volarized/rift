//! The layer that prints records on standard error, and standard error of a detached
//! server, cut at a byte bound.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::capture::{closed_record, event_record};
use crate::metrics::{Counter, SCOPE};
use crate::record::LogRecord;
use crate::render::{LevelColor, LiveLine, LogLines};

/// The `tracing` layer that prints each admitted event and span close as one line of a
/// live stream on its writer: the line [`LogLines::live_stream`] prints for the record the
/// log capture stores, a blank line before it when its group differs from the line before.
///
/// It builds the record the log capture builds, from the span context
/// [`SpanContextLayer`](crate::capture::SpanContextLayer) keeps, and writes the line in one
/// `write_all` call.
pub(crate) struct StderrLines<W> {
    writer: W,
    color: LevelColor,
    lines: Mutex<LogLines>,
}

impl<W> StderrLines<W> {
    /// Lines on `writer`, the level in `color`.
    pub(crate) const fn new(writer: W, color: LevelColor) -> Self {
        Self {
            writer,
            color,
            lines: Mutex::new(LogLines::live_stream()),
        }
    }
}

impl<W> StderrLines<W>
where
    W: for<'writer> MakeWriter<'writer> + 'static,
{
    /// Writes `record` as one line. The line renders before the lock; placing it after
    /// the group of the last line and the write happen under the lock, so two threads
    /// never print a blank line for each other's group. A failed write is dropped: stderr
    /// has no reader to report it to.
    fn print(&self, record: &LogRecord) {
        let line = LiveLine::of(record, self.color);
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        let text = lines.placed(line);
        let _ = self.writer.make_writer().write_all(text.as_bytes());
    }
}

impl<S, W> Layer<S> for StderrLines<W>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    W: for<'writer> MakeWriter<'writer> + 'static,
{
    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        if let Some(record) = closed_record(&id, &context) {
            self.print(&record);
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        self.print(&event_record(event, &context));
    }
}

/// Bytes of traced diagnostics a server writes to its standard error before it stops
/// writing there, when that stream is not a terminal and `[logs] stderr_limit` set no
/// other bound.
///
/// The file `rift server start` hands its server is what a crashed server
/// leaves behind: it holds the start, and a panic's own report reaches it
/// through the default panic hook past this bound. The diagnostics of a long
/// life go to `rift server logs`.
pub const SERVER_STDERR_BYTES_MAX: u64 = 1 << 20;
/// The line the writer prints once, as the last thing, when the bound is reached.
const SERVER_STDERR_BOUND_NOTICE: &str =
    "rift: standard error reached its byte bound; later diagnostics are under `rift server logs`\n";
/// `log.stderr.discarded`: the bytes the bounded standard error discarded past
/// `[logs] stderr_limit`.
static LOG_STDERR_DISCARDED: Counter<0> =
    Counter::declare(SCOPE, "log.stderr.discarded", "By", &[]);

/// The byte bound of a server's standard error, and what passed and was discarded at it.
///
/// Every writer of one [`BoundedStderr`] shares it, and the log drain's stop reads
/// [`Self::discarded`] from it.
#[derive(Debug)]
pub(crate) struct StderrBound {
    limit: u64,
    written: AtomicU64,
    discarded: AtomicU64,
}

impl StderrBound {
    /// A bound of `limit` bytes, nothing written yet.
    pub(crate) const fn new(limit: u64) -> Self {
        Self {
            limit,
            written: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
        }
    }

    /// The bound in bytes: the accepted `[logs] stderr_limit`.
    pub(crate) const fn limit(&self) -> u64 {
        self.limit
    }

    /// The bytes discarded past the bound so far.
    pub(crate) fn discarded(&self) -> u64 {
        self.discarded.load(Ordering::Relaxed)
    }
}

/// Standard error of a server whose stream is a file, cut at its [`StderrBound`].
///
/// The file `rift server start` hands its server would otherwise grow for
/// the server's whole life. Past the bound the writer prints one notice and
/// discards what it is handed afterwards, counting the bytes in the bound and in
/// `log.stderr.discarded`; the diagnostics recorded under `rift server logs` are
/// unaffected.
#[derive(Debug)]
pub(crate) struct BoundedStderr {
    bound: Arc<StderrBound>,
}

impl BoundedStderr {
    /// Standard error cut at `bound`.
    pub(crate) const fn new(bound: Arc<StderrBound>) -> Self {
        Self { bound }
    }
}

impl<'a> MakeWriter<'a> for BoundedStderr {
    type Writer = BoundedWriter<'a, io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        BoundedWriter::new(&self.bound, io::stderr())
    }
}

/// One writer over a shared [`StderrBound`]: writes pass through until the count
/// reaches the bound, the crossing write is followed by the notice, and later writes
/// are counted, in the bound and in `log.stderr.discarded`, and discarded.
#[derive(Debug)]
pub(crate) struct BoundedWriter<'a, Sink: Write> {
    bound: &'a StderrBound,
    sink: Sink,
}

impl<'a, Sink: Write> BoundedWriter<'a, Sink> {
    /// A writer over `sink` sharing `bound` with every sibling writer.
    pub(crate) const fn new(bound: &'a StderrBound, sink: Sink) -> Self {
        Self { bound, sink }
    }
}

impl<Sink: Write> Write for BoundedWriter<'_, Sink> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = bytes.len() as u64;
        let before = self.bound.written.fetch_add(length, Ordering::Relaxed);
        if before >= self.bound.limit {
            self.bound.discarded.fetch_add(length, Ordering::Relaxed);
            LOG_STDERR_DISCARDED.add(length);
            return Ok(bytes.len());
        }
        self.sink.write_all(bytes)?;
        if before + length >= self.bound.limit {
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
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{
        BoundedWriter, SERVER_STDERR_BOUND_NOTICE, SERVER_STDERR_BYTES_MAX, StderrBound,
        StderrLines,
    };
    use crate::SeriesValue;
    use crate::render::{LevelColor, LogLines};

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
        /// Everything printed, with the times that vary between runs read as `_`.
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the written bytes are not poisoned");
            without_times(&String::from_utf8_lossy(&bytes))
        }
    }

    /// `text` with each line's timestamp, which must be UTC, and each `busy` and `idle`
    /// value read as `_`: they vary between runs, and every other column is exact.
    fn without_times(text: &str) -> String {
        let mut lines = Vec::new();
        for line in text.split('\n') {
            if line.is_empty() {
                lines.push(String::new());
                continue;
            }
            let mut parts = line.splitn(3, ' ');
            let (date, time, rest) = (parts.next(), parts.next(), parts.next());
            assert!(
                date.is_some_and(|date| date.len() == 10)
                    && time.is_some_and(|time| time.ends_with('Z')),
                "{line}"
            );
            let rest = rest
                .unwrap_or_default()
                .split(' ')
                .map(|part| match part.split_once('=') {
                    Some((key @ ("busy" | "idle"), _)) => format!("{key}=_"),
                    _ => part.to_owned(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!("_ {rest}"));
        }
        lines.join("\n")
    }

    /// Runs `emit` under a subscriber whose stderr lines go to the returned buffer.
    fn printed(emit: impl FnOnce()) -> Written {
        crate::__private::stream_unscoped();
        let written = Written::default();
        let writer = written.clone();
        let layer = StderrLines::new(move || writer.clone(), LevelColor::Plain).with_filter(
            crate::runtime::stderr_filter(EnvFilter::new("rift_tracing=trace,hidden=trace")),
        );
        let runtime = crate::recorder::TestOtlpRuntime::when_configured_without_runtime();
        let (otlp_layer, export) = if let Some(runtime) = &runtime {
            let _entered = runtime.enter();
            crate::otlp::test_process_layer(EnvFilter::new("trace"))
        } else {
            crate::otlp::test_process_layer(EnvFilter::new("trace"))
        };
        tracing::subscriber::with_default(
            crate::capture::registry().with(layer).with(otlp_layer),
            emit,
        );
        if let Some(runtime) = runtime {
            runtime
                .shutdown(export)
                .expect("the local test OTLP export shuts down");
        }
        written
    }

    /// The owner's case: an index operation inside a `tools/call` request.
    fn request_with_index_operation() {
        let request = crate::info_span!(
            "mcp.request",
            component = "mcp",
            operation = "tools/call",
            request_id = 18,
            tool = "get_symbol"
        );
        request.in_scope(|| {
            crate::traced!(component = "index", operation = "fingerprint.discover", {
                crate::info!(files = 12, "walked the workspace");
            });
        });
    }

    /// The function that opens the request of [`request_with_index_operation`].
    const REQUEST_FUNCTION: &str = "rift_tracing::stderr::tests::request_with_index_operation";

    #[test]
    fn an_event_outside_every_span_prints_its_function_context_and_message() {
        let text = printed(|| {
            crate::info!(component = "mcp", transport = "http", "MCP server starting");
        })
        .text();

        assert_eq!(
            text,
            format!(
                "_ INFO  {:<36}   {:<48}  MCP server starting\n",
                "rift_tracing::stderr::tests::\
                 an_event_outside_every_span_prints_its_function_context_and_message",
                "component=mcp transport=http",
            )
        );
    }

    /// The event and both closes of one request: the request's context on every line, the
    /// nested operation after `↳`, and the request's own close under `close ✓`.
    #[test]
    fn a_request_prints_its_context_on_every_line_and_its_closes() {
        let text = printed(request_with_index_operation).text();

        let context = "component=mcp operation=tools/call req=18 tool=get_symbol";
        let nested = format!(
            "↳ {:<24} {:<40} ",
            "fingerprint.discover", "component=index operation=fingerprint.discover"
        );
        assert_eq!(
            text,
            format!(
                "_ INFO  {REQUEST_FUNCTION:<36}   {context:<48}  {nested}walked the workspace \
                 files=12\n\
                 _ INFO  {REQUEST_FUNCTION:<36}   {context:<48}  {nested}close ✓ busy=_ idle=_\n\
                 _ INFO  {REQUEST_FUNCTION:<36}   {context:<48}  close ✓ busy=_ idle=_\n"
            )
        );
    }

    /// A live stream prints a blank line where the group changes: from one request to the
    /// next, and from a request to a record outside every span.
    #[test]
    fn a_live_stream_breaks_where_the_group_changes() {
        let text = printed(|| {
            request_with_index_operation();
            request_with_index_operation();
            crate::warn!(component = "index", reason = "shutdown", "stopped");
        })
        .text();

        let blank = text.split('\n').filter(|line| line.is_empty()).count();
        assert_eq!(
            blank, 2,
            "one break before the warning, one at the end: {text}"
        );
        assert!(text.contains("\n\n_ WARN"), "{text}");
    }

    /// Stderr and `rift server logs --follow` print one form: the stderr line of each
    /// record is the line the captured record renders to on a live stream, also for an
    /// event inside a span the capture filter leaves out, which the stored record still
    /// names. That span's own close reaches stderr alone.
    #[test]
    fn a_stderr_line_is_the_live_line_of_the_captured_record() {
        crate::__private::stream_unscoped();
        let written = Written::default();
        let writer = written.clone();
        let (sink, mut drain) = crate::log_capture();
        let capture_filter = EnvFilter::new("rift_tracing=trace,hidden=off");
        let runtime = crate::recorder::TestOtlpRuntime::when_configured_without_runtime();
        let (otlp_layer, export) = if let Some(runtime) = &runtime {
            let _entered = runtime.enter();
            crate::otlp::test_process_layer(capture_filter.clone())
        } else {
            crate::otlp::test_process_layer(capture_filter.clone())
        };
        let subscriber = crate::capture::registry()
            .with(
                StderrLines::new(move || writer.clone(), LevelColor::Plain).with_filter(
                    crate::runtime::stderr_filter(EnvFilter::new(
                        "rift_tracing=trace,hidden=trace",
                    )),
                ),
            )
            .with(crate::runtime::capture_layer(sink, capture_filter))
            .with(otlp_layer);
        tracing::subscriber::with_default(subscriber, || {
            request_with_index_operation();
            let hidden = tracing::info_span!(
                target: "hidden",
                "dependency.context",
                component = "dependency",
                operation = "dependency.context",
            );
            hidden.in_scope(|| crate::info!("operation opened"));
            drop(hidden);
            crate::warn!(component = "index", reason = "shutdown", "stopped");
        });
        if let Some(runtime) = runtime {
            runtime
                .shutdown(export)
                .expect("the local test OTLP export shuts down");
        }

        let stored = std::iter::from_fn(|| drain.try_recv_record().ok()).collect::<Vec<_>>();
        let rendered = without_times(&LogLines::live_stream().lines(&stored));
        let printed = written.text();
        let records = |text: &str| {
            text.split('\n')
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let mut expected = records(&printed);
        let hidden_close = expected
            .iter()
            .position(|line| {
                line.contains("operation=dependency.context") && line.contains("close ✓")
            })
            .expect("stderr prints the left-out span's close");
        expected.remove(hidden_close);
        assert_eq!(records(&rendered), expected);
        assert!(
            rendered.contains("component=dependency operation=dependency.context"),
            "the stored event names its span: {rendered}"
        );
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
        let bound = StderrBound::new(SERVER_STDERR_BYTES_MAX);
        let mut sink = Vec::new();
        let head = vec![b'a'; usize::try_from(SERVER_STDERR_BYTES_MAX)? - 4];
        {
            let mut writer = BoundedWriter::new(&bound, &mut sink);
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
            bound.written.load(Ordering::Relaxed),
            (head.len() + "crossing".len() + "dropped".len()) as u64,
            "dropped bytes are still counted"
        );
        assert_eq!(bound.discarded(), "dropped".len() as u64);
        Ok(())
    }

    #[test]
    fn a_bounded_writer_shares_its_count_between_writers() -> TestResult {
        let bound = StderrBound::new(SERVER_STDERR_BYTES_MAX);
        bound
            .written
            .store(SERVER_STDERR_BYTES_MAX, Ordering::Relaxed);
        let mut sink = Vec::new();
        BoundedWriter::new(&bound, &mut sink).write_all(b"late")?;
        assert!(sink.is_empty(), "a writer past the bound writes nothing");
        assert_eq!(bound.discarded(), 4);
        Ok(())
    }

    /// The bound is the accepted `[logs] stderr_limit`: the writer passes that many bytes,
    /// discards the rest, and counts the discarded bytes in `log.stderr.discarded`.
    #[test]
    fn a_bounded_writer_stops_at_its_limit_and_counts_what_it_discards() -> TestResult {
        let (recorder, _drain) = crate::ScopedRecorder::builder().install()?;
        let bound = StderrBound::new(16);
        let mut sink = Vec::new();
        {
            let mut writer = BoundedWriter::new(&bound, &mut sink);
            writer.write_all(b"0123456789")?;
            writer.write_all(b"abcdefgh")?;
            writer.write_all(b"discarded")?;
            writer.write_all(b"also")?;
        }
        assert_eq!(bound.limit(), 16);
        assert_eq!(
            sink,
            [
                b"0123456789abcdefgh".as_slice(),
                SERVER_STDERR_BOUND_NOTICE.as_bytes()
            ]
            .concat()
        );
        assert_eq!(bound.discarded(), 13);
        let snapshot = recorder.metrics();
        let series = snapshot
            .find("log.stderr.discarded", &[])
            .ok_or("the discarded bytes are counted")?;
        assert_eq!(series.value(), &SeriesValue::Sum(13.0));
        assert_eq!(series.unit(), "By");
        Ok(())
    }

    /// The log drain's stop records the bytes the bounded standard error discarded, with
    /// the bound: `WARN` past the bound, `INFO` with a count of zero below it.
    #[tokio::test]
    async fn a_drain_stop_records_the_bytes_its_bounded_stderr_discarded() -> TestResult {
        for (written, level, discarded) in [
            (&[b"crossing".as_slice(), b"discarded"][..], "warn", "9"),
            (&[b"ok".as_slice()][..], "info", "0"),
        ] {
            let directory = tempfile::tempdir()?;
            let store =
                Arc::new(crate::LogStore::open(&directory.path().join("metrics"), None).await?);
            let bound = Arc::new(StderrBound::new(4));
            for bytes in written {
                BoundedWriter::new(&bound, Vec::new()).write_all(bytes)?;
            }
            let (_sink, drain) = crate::log_capture();
            let running =
                crate::RunningLogDrain::spawn(drain.with_stderr(Arc::clone(&bound)), store, 100);
            let (recorder, mut records) = crate::ScopedRecorder::builder().install()?;
            running
                .stop(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                .await;
            drop(recorder);

            let record = records
                .queued_records()
                .into_iter()
                .find(|record| record.message() == "standard error bytes discarded")
                .ok_or("the stop records the discarded bytes")?;
            assert_eq!(record.level(), level);
            let fields: serde_json::Value = serde_json::from_str(record.fields())?;
            assert_eq!(fields["stderr_limit"], "4", "{fields}");
            assert_eq!(fields["discarded"], discarded, "{fields}");
        }
        Ok(())
    }
}
