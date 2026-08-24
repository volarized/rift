//! Byte framing for the LSP base protocol.
//!
//! Every message an engine sends or receives is a header block of
//! `Name: value` lines ended by a blank line, then exactly `Content-Length`
//! bytes of JSON. The codec here turns fed bytes into complete JSON payloads
//! and wraps outgoing payloads in the header, without performing any I/O.

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, LimitEvidence, fault_label};
use serde::Serialize;

/// Maximum bytes in one message's header block, terminator included.
pub const HEADER_BYTES_MAX: usize = 4 << 10;

/// Maximum bytes in one message body an engine may announce.
pub const MESSAGE_BYTES_MAX: usize = 64 << 20;

/// The header naming the body size, compared ASCII case-insensitively.
const CONTENT_LENGTH_HEADER: &str = "content-length";

/// The blank line ending a header block.
const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

/// The line ending separating header lines.
const HEADER_LINE_ENDING: &str = "\r\n";

/// The separator between a header name and its value.
const HEADER_SEPARATOR: char = ':';

/// How one byte stream broke the base protocol.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FramingFault {
    /// The header block ran past [`HEADER_BYTES_MAX`] without its blank line.
    HeaderTooLong,
    /// The header block is not ASCII `Name: value` lines.
    HeaderMalformed,
    /// The header block carries no `Content-Length`.
    ContentLengthMissing,
    /// The `Content-Length` value is not a decimal byte count.
    ContentLengthInvalid {
        /// The value as received.
        value: String,
    },
    /// The announced body size crosses [`MESSAGE_BYTES_MAX`].
    MessageTooLong {
        /// The body size the header announced.
        announced_bytes: usize,
    },
}

impl Fault for FramingFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::HeaderTooLong | Self::MessageTooLong { .. } => {
                ErrorName::Wire(ErrorCode::LimitExceeded)
            }
            _ => ErrorName::Wire(ErrorCode::CapabilityUnavailable),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("fault", fault_label(self))];
        match self {
            Self::ContentLengthInvalid { value } => {
                context.push(ErrorContext::new("value", value.clone()));
            }
            Self::MessageTooLong { announced_bytes } => {
                context.push(ErrorContext::new(
                    "announced_bytes",
                    announced_bytes.to_string(),
                ));
            }
            _ => {}
        }
        context
    }

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        match self {
            Self::MessageTooLong { announced_bytes } => Some(LimitEvidence {
                field: "framing.message_bytes_max".to_owned(),
                limit: u64::try_from(MESSAGE_BYTES_MAX).unwrap_or(u64::MAX),
                required: u64::try_from(*announced_bytes).unwrap_or(u64::MAX),
            }),
            _ => None,
        }
    }
}

/// A byte stream that broke the base protocol.
pub type FramingError = Error<FramingFault>;

/// Incremental decoder and encoder for base-protocol frames.
///
/// Feed bytes in arrival order; complete JSON payloads come back in the same
/// order. Buffered bytes stay below `HEADER_BYTES_MAX + MESSAGE_BYTES_MAX`
/// plus one fed chunk, because every complete frame is drained on the feed
/// that completes it and an overlong header or body is refused instead of
/// buffered.
#[derive(Debug, Default)]
pub struct Framing {
    buffer: Vec<u8>,
}

impl Framing {
    /// An empty codec awaiting its first bytes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps one outgoing JSON payload in its `Content-Length` header.
    ///
    /// Outgoing payloads are Rift-authored requests and responses; their
    /// size is bounded by the document and parameter bounds upstream, so
    /// emission is infallible.
    #[must_use]
    pub fn frame(payload: &[u8]) -> Vec<u8> {
        let header = format!(
            "Content-Length{HEADER_SEPARATOR} {}{HEADER_LINE_ENDING}{HEADER_LINE_ENDING}",
            payload.len()
        );
        let mut framed = Vec::with_capacity(header.len() + payload.len());
        framed.extend_from_slice(header.as_bytes());
        framed.extend_from_slice(payload);
        framed
    }

    /// Buffers `bytes` and returns every message they complete, in order.
    ///
    /// The drain loop runs at most once per completed frame, and a frame is
    /// never shorter than its own header, so iterations are bounded by the
    /// buffered byte count.
    ///
    /// # Errors
    ///
    /// Returns [`FramingError`] when the stream breaks the base protocol;
    /// the codec is then unusable and the engine must be ended.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, FramingError> {
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        while let Some(message) = self.next_complete()? {
            messages.push(message);
        }
        Ok(messages)
    }

    /// Extracts the next complete frame from the buffer, if one is there.
    fn next_complete(&mut self) -> Result<Option<Vec<u8>>, FramingError> {
        let Some(terminator) = find(&self.buffer, HEADER_TERMINATOR) else {
            if self.buffer.len() > HEADER_BYTES_MAX {
                return Err(Error::new(FramingFault::HeaderTooLong));
            }
            return Ok(None);
        };
        let header_end = terminator + HEADER_TERMINATOR.len();
        if header_end > HEADER_BYTES_MAX {
            return Err(Error::new(FramingFault::HeaderTooLong));
        }
        let body_bytes = content_length(&self.buffer[..terminator])?;
        if body_bytes > MESSAGE_BYTES_MAX {
            return Err(Error::new(FramingFault::MessageTooLong {
                announced_bytes: body_bytes,
            }));
        }
        if self.buffer.len() < header_end + body_bytes {
            return Ok(None);
        }
        let message = self.buffer[header_end..header_end + body_bytes].to_vec();
        self.buffer.drain(..header_end + body_bytes);
        Ok(Some(message))
    }
}

/// The announced body size from one header block.
fn content_length(header: &[u8]) -> Result<usize, FramingError> {
    let header =
        std::str::from_utf8(header).map_err(|_| Error::new(FramingFault::HeaderMalformed))?;
    for line in header.split(HEADER_LINE_ENDING) {
        let Some((name, value)) = line.split_once(HEADER_SEPARATOR) else {
            return Err(Error::new(FramingFault::HeaderMalformed));
        };
        if name.trim().eq_ignore_ascii_case(CONTENT_LENGTH_HEADER) {
            let value = value.trim();
            return value.parse().map_err(|_| {
                Error::new(FramingFault::ContentLengthInvalid {
                    value: value.to_owned(),
                })
            });
        }
    }
    Err(Error::new(FramingFault::ContentLengthMissing))
}

/// The first position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(codec: &mut Framing, bytes: &[u8]) -> Vec<Vec<u8>> {
        codec.feed(bytes).expect("feed must succeed")
    }

    #[test]
    fn frame_and_feed_round_trip_one_message() {
        let framed = Framing::frame(b"{\"x\":1}");
        assert_eq!(framed, b"Content-Length: 7\r\n\r\n{\"x\":1}");
        let mut codec = Framing::new();
        assert_eq!(fed(&mut codec, &framed), [b"{\"x\":1}".to_vec()]);
    }

    #[test]
    fn feed_assembles_messages_split_across_arbitrary_chunks() {
        let framed = Framing::frame(br#"{"method":"initialized"}"#);
        for split in 1..framed.len() {
            let mut codec = Framing::new();
            assert_eq!(fed(&mut codec, &framed[..split]), Vec::<Vec<u8>>::new());
            assert_eq!(
                fed(&mut codec, &framed[split..]),
                [br#"{"method":"initialized"}"#.to_vec()]
            );
        }
    }

    #[test]
    fn feed_returns_two_messages_from_one_chunk_in_order() {
        let mut chunk = Framing::frame(b"{\"a\":1}");
        chunk.extend_from_slice(&Framing::frame(b"{\"b\":2}"));
        let mut codec = Framing::new();
        assert_eq!(
            fed(&mut codec, &chunk),
            [b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]
        );
    }

    #[test]
    fn header_names_are_case_insensitive_and_extra_headers_are_ignored() {
        let mut codec = Framing::new();
        let framed = b"Content-Type: application/json\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(fed(&mut codec, framed), [b"{}".to_vec()]);
    }

    #[test]
    fn garbage_without_a_header_separator_is_refused() {
        let mut codec = Framing::new();
        let error = codec
            .feed(b"not a header\r\n\r\n")
            .expect_err("garbage must be refused");
        assert_eq!(*error.fault(), FramingFault::HeaderMalformed);
    }

    #[test]
    fn missing_and_invalid_content_length_are_distinct_refusals() {
        let missing = Framing::new()
            .feed(b"Content-Type: application/json\r\n\r\n")
            .expect_err("missing length must be refused");
        assert_eq!(*missing.fault(), FramingFault::ContentLengthMissing);
        let invalid = Framing::new()
            .feed(b"Content-Length: many\r\n\r\n")
            .expect_err("non-decimal length must be refused");
        assert_eq!(
            *invalid.fault(),
            FramingFault::ContentLengthInvalid {
                value: "many".to_owned()
            }
        );
    }

    #[test]
    fn header_past_its_bound_is_refused_with_and_without_terminator() {
        let unterminated = Framing::new()
            .feed(&vec![b'a'; HEADER_BYTES_MAX + 1])
            .expect_err("endless header must be refused");
        assert_eq!(*unterminated.fault(), FramingFault::HeaderTooLong);
        let mut oversized = format!("Content-Length: 2{}", "\r\nA: b".repeat(900)).into_bytes();
        oversized.extend_from_slice(b"\r\n\r\n{}");
        let terminated = Framing::new()
            .feed(&oversized)
            .expect_err("oversized header must be refused");
        assert_eq!(*terminated.fault(), FramingFault::HeaderTooLong);
    }

    #[test]
    fn announced_body_past_its_bound_is_refused_with_limit_evidence() {
        let announced = MESSAGE_BYTES_MAX + 1;
        let error = Framing::new()
            .feed(format!("Content-Length: {announced}\r\n\r\n").as_bytes())
            .expect_err("oversized body must be refused");
        assert_eq!(
            *error.fault(),
            FramingFault::MessageTooLong {
                announced_bytes: announced
            }
        );
        assert!(error.fault().limit_evidence().is_some());
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::LimitExceeded));
    }

    #[test]
    fn non_utf8_header_bytes_are_refused_as_malformed() {
        let error = Framing::new()
            .feed(b"Content-Length\xff: 2\r\n\r\n{}")
            .expect_err("non-ASCII header must be refused");
        assert_eq!(*error.fault(), FramingFault::HeaderMalformed);
    }

    #[test]
    fn fault_rendering_names_the_evidence_for_every_variant() {
        let malformed = Error::new(FramingFault::HeaderMalformed);
        assert_eq!(
            malformed.name(),
            ErrorName::Wire(ErrorCode::CapabilityUnavailable)
        );
        assert!(malformed.fault().limit_evidence().is_none());
        assert!(malformed.to_string().contains("header_malformed"));
        let missing = Error::new(FramingFault::ContentLengthMissing);
        assert!(missing.to_string().contains("content_length_missing"));
        let invalid = Error::new(FramingFault::ContentLengthInvalid {
            value: "many".to_owned(),
        });
        assert!(invalid.to_string().contains("value many"));
        let announced = Error::new(FramingFault::MessageTooLong { announced_bytes: 7 });
        assert!(announced.to_string().contains("announced_bytes 7"));
        let overlong = Error::new(FramingFault::HeaderTooLong);
        assert_eq!(overlong.name(), ErrorName::Wire(ErrorCode::LimitExceeded));
        assert!(overlong.fault().limit_evidence().is_none());
    }

    #[test]
    fn body_at_exactly_the_bound_is_accepted() {
        let payload = vec![b'x'; MESSAGE_BYTES_MAX];
        let mut codec = Framing::new();
        let messages = fed(&mut codec, &Framing::frame(&payload));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].len(), MESSAGE_BYTES_MAX);
    }
}
