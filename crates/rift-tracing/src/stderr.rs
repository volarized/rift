//! Standard error of a detached server, cut at a byte bound.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use tracing_subscriber::fmt::MakeWriter;

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

    use super::{BoundedWriter, SERVER_STDERR_BOUND_NOTICE, SERVER_STDERR_BYTES_MAX};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

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
