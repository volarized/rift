//! Bounded JSON message bytes shared by transport owners.

use std::io::{self, Write};

use serde::Serialize;

/// Encodes a message without appending bytes beyond the accepted bound.
///
/// # Errors
/// Returns a serialization, allocation or byte-bound failure.
pub fn encode_bounded(
    message: &impl Serialize,
    limit: usize,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut buffer = MessageBuffer::new(limit);
    serde_json::to_writer(&mut buffer, message)?;
    Ok(buffer.into_bytes())
}

/// A message buffer that admits each write before allocation and append.
pub struct MessageBuffer {
    bytes: Vec<u8>,
    limit: usize,
    rejected_bytes: Option<usize>,
}

impl MessageBuffer {
    /// Creates an empty buffer under the accepted byte bound.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            rejected_bytes: None,
        }
    }

    /// Returns the admitted bytes without copying them.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the owned admitted bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Returns the first refused prefix length, saturated at the platform bound.
    /// This is a lower bound on the complete encoded message length.
    #[must_use]
    pub const fn rejected_bytes(&self) -> Option<usize> {
        self.rejected_bytes
    }
}

impl Write for MessageBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = next_length(self.bytes.len(), bytes.len(), self.limit).ok_or_else(|| {
            self.rejected_bytes
                .get_or_insert(self.bytes.len().saturating_add(bytes.len()));
            io::Error::other("JSON-RPC message exceeds byte limit")
        })?;
        self.bytes
            .try_reserve_exact(next - self.bytes.len())
            .map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn next_length(current: usize, incoming: usize, limit: usize) -> Option<usize> {
    current.checked_add(incoming).filter(|next| *next <= limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_buffer_admits_exact_bytes_and_refuses_crossing_chunk() {
        let mut buffer = MessageBuffer::new(4);
        buffer.write_all(b"abc").expect("admitted prefix");
        assert!(buffer.write_all(b"de").is_err());
        assert_eq!(buffer.as_bytes(), b"abc");
        assert_eq!(buffer.rejected_bytes(), Some(5));
        buffer.write_all(b"d").expect("exact bound");
        assert_eq!(buffer.into_bytes(), b"abcd");
        assert_eq!(next_length(usize::MAX, 1, usize::MAX), None);
        assert_eq!(next_length(3, 1, 4), Some(4));
    }

    #[test]
    fn bounded_message_encoding_preserves_json_and_exact_byte_bound() {
        let value = serde_json::json!({"message": "selected source"});
        let expected = serde_json::to_vec(&value).expect("JSON bytes");
        assert_eq!(
            encode_bounded(&value, expected.len()).expect("exact bound"),
            expected
        );
        assert!(encode_bounded(&value, expected.len() - 1).is_err());
    }
}
