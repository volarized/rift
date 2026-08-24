//! Bounded capture vocabulary for child-process output streams.
//!
//! Hook runs and language engine sessions drain a child's pipes under the
//! same policy: keep a configured prefix, count the rest up to a ceiling,
//! and report both so a truncated log is distinguishable from a short one.

/// Bytes read from a child stream per read call.
pub const STREAM_READ_BYTES: usize = 8 << 10;

/// Bytes of one child stream a drain counts before it stops reading. A
/// child that produces more blocks on its full pipe until its timeout
/// kills it, and the reported total stays at this ceiling.
pub const STREAM_TOTAL_BYTES_MAX: u64 = 64 << 20;

/// One captured output stream: a bounded prefix plus the full size.
///
/// Reporting both keeps a truncated log distinguishable from a short one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapturedStream {
    /// The captured prefix, lossily decoded as UTF-8.
    pub text: String,
    /// Bytes of the prefix actually captured.
    pub captured_bytes: u64,
    /// Bytes the stream produced, counted up to the drain ceiling.
    pub total_bytes: u64,
    /// Whether the capture stopped short of the full stream.
    pub truncated: bool,
}
