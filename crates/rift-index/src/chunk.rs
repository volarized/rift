//! Splits one text file's content into size-bounded chunks for lexical indexing.
//!
//! A chunk packs whole lines greedily up to a byte bound, preserving each line's exact ending
//! through [`rift_core::line::lines_inclusive`] so CRLF content stays byte-identical when its
//! chunks are reconcatenated. A single line longer than the bound is hard-split at UTF-8
//! character boundaries into bound-sized pieces, since a line cannot otherwise be shortened
//! without losing bytes. Splitting is deterministic and every chunk's `byte_offset` is its
//! exact start position in the original content.

use rift_core::line::lines_inclusive;

/// One size-bounded slice of a text file's content, with its exact byte offset into the
/// whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextChunk {
    byte_offset: u64,
    content: String,
}

impl TextChunk {
    fn new(byte_offset: u64, content: String) -> Self {
        Self {
            byte_offset,
            content,
        }
    }

    /// Returns this chunk's start position in the file it was split from.
    #[must_use]
    pub(crate) const fn byte_offset(&self) -> u64 {
        self.byte_offset
    }

    /// Returns this chunk's text.
    #[must_use]
    pub(crate) fn content(&self) -> &str {
        &self.content
    }
}

/// Splits `content` into chunks of at most `chunk_bytes_max` bytes each, in order.
/// Concatenating every returned chunk's content reproduces `content` byte-for-byte.
///
/// Returns an empty list for empty `content`.
///
/// # Panics
///
/// Asserts `chunk_bytes_max` is positive: every caller passes an already-accepted bound.
#[must_use]
pub(crate) fn text_chunks(content: &str, chunk_bytes_max: usize) -> Vec<TextChunk> {
    assert!(
        chunk_bytes_max > 0,
        "chunk_bytes_max must be positive: chunk_bytes_max={chunk_bytes_max}"
    );
    let mut chunks = Vec::new();
    let mut buffer = String::new();
    let mut buffer_start: u64 = 0;
    let mut cursor: u64 = 0;
    for line in lines_inclusive(content) {
        let line_bytes = byte_length(line);
        if line.len() > chunk_bytes_max {
            flush_buffer(&mut chunks, &mut buffer, buffer_start);
            for piece in hard_split_line(line, chunk_bytes_max) {
                let piece_bytes = byte_length(piece);
                chunks.push(TextChunk::new(cursor, piece.to_owned()));
                cursor += piece_bytes;
            }
            buffer_start = cursor;
            continue;
        }
        if buffer.is_empty() {
            buffer_start = cursor;
        } else if buffer.len() + line.len() > chunk_bytes_max {
            flush_buffer(&mut chunks, &mut buffer, buffer_start);
            buffer_start = cursor;
        }
        buffer.push_str(line);
        cursor += line_bytes;
    }
    flush_buffer(&mut chunks, &mut buffer, buffer_start);
    chunks
}

/// Widens a line or piece's byte length into the `u64` domain a chunk offset is tracked in.
fn byte_length(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

/// Appends the accumulated `buffer` as one chunk starting at `start`, when non-empty.
fn flush_buffer(chunks: &mut Vec<TextChunk>, buffer: &mut String, start: u64) {
    if !buffer.is_empty() {
        chunks.push(TextChunk::new(start, std::mem::take(buffer)));
    }
}

/// Splits one line longer than `chunk_bytes_max` into bound-sized pieces, cutting only at
/// UTF-8 character boundaries. A single character wider than `chunk_bytes_max` still forms
/// its own piece, since a character cannot be split without producing invalid UTF-8.
fn hard_split_line(line: &str, chunk_bytes_max: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut remainder = line;
    while !remainder.is_empty() {
        let boundary = char_boundary_at_or_before(remainder, chunk_bytes_max);
        let (piece, rest) = remainder.split_at(boundary);
        pieces.push(piece);
        remainder = rest;
    }
    pieces
}

/// The largest char boundary in `text` at or before `max` bytes, never zero: a single
/// character exceeding `max` still yields itself whole.
fn char_boundary_at_or_before(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut boundary = max;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    if boundary == 0 {
        boundary = text.chars().next().map_or(text.len(), char::len_utf8);
    }
    boundary
}

#[cfg(test)]
mod tests {
    use super::{TextChunk, text_chunks};

    fn contents(chunks: &[TextChunk]) -> Vec<&str> {
        chunks.iter().map(TextChunk::content).collect()
    }

    fn offsets(chunks: &[TextChunk]) -> Vec<u64> {
        chunks.iter().map(TextChunk::byte_offset).collect()
    }

    #[test]
    fn test_text_chunks_of_empty_content_is_empty() {
        assert!(text_chunks("", 16).is_empty());
    }

    #[test]
    fn test_text_chunks_of_small_content_is_one_chunk_at_offset_zero() {
        let chunks = text_chunks("one\ntwo\n", 1_024);
        assert_eq!(contents(&chunks), ["one\ntwo\n"]);
        assert_eq!(offsets(&chunks), [0]);
    }

    #[test]
    fn test_text_chunks_packs_lines_up_to_the_exact_boundary() {
        // Each line is 4 bytes ("aa\n\"-style); two lines exactly fill an 8-byte bound and
        // must land in one chunk, not two.
        let content = "aaa\nbbb\n";
        let chunks = text_chunks(content, 8);
        assert_eq!(contents(&chunks), ["aaa\nbbb\n"]);
    }

    #[test]
    fn test_text_chunks_splits_once_the_next_line_would_cross_the_bound() {
        let content = "aaa\nbbb\nccc\n";
        let chunks = text_chunks(content, 8);
        assert_eq!(contents(&chunks), ["aaa\nbbb\n", "ccc\n"]);
    }

    #[test]
    fn test_text_chunks_offsets_are_contiguous_across_several_chunks() {
        let content = "aaa\nbbb\nccc\nddd\n";
        let chunks = text_chunks(content, 8);
        let mut expected_offset = 0_u64;
        for chunk in &chunks {
            assert_eq!(chunk.byte_offset(), expected_offset);
            expected_offset += u64::try_from(chunk.content().len()).expect("fits u64");
        }
        assert_eq!(
            expected_offset,
            u64::try_from(content.len()).expect("fits u64")
        );
    }

    #[test]
    fn test_text_chunks_hard_splits_a_single_oversized_line() {
        let content = "abcdefghij\n";
        let chunks = text_chunks(content, 4);
        assert_eq!(contents(&chunks), ["abcd", "efgh", "ij\n"]);
        assert_eq!(offsets(&chunks), [0, 4, 8]);
    }

    #[test]
    fn test_text_chunks_flushes_pending_buffer_before_hard_splitting() {
        let content = "a\nbbbbbbbbbb\n";
        let chunks = text_chunks(content, 4);
        assert_eq!(contents(&chunks), ["a\n", "bbbb", "bbbb", "bb\n"]);
    }

    #[test]
    fn test_text_chunks_reconcatenation_is_byte_identical_for_crlf_content() {
        let content = "one\r\ntwo\r\nthree\r\nfour\r\n";
        let chunks = text_chunks(content, 10);
        let rejoined: String = chunks.iter().map(TextChunk::content).collect();
        assert_eq!(
            rejoined, content,
            "CRLF bytes must survive chunking exactly"
        );
    }

    #[test]
    fn test_text_chunks_hard_split_never_breaks_a_multi_byte_character() {
        // 'é' is 2 bytes in UTF-8; a boundary landing mid-character would make `split_at`
        // panic, so a byte-identical rejoin proves every cut fell on a character boundary.
        let content = "caf\u{e9}caf\u{e9}\n";
        let chunks = text_chunks(content, 3);
        let rejoined: String = chunks.iter().map(TextChunk::content).collect();
        assert_eq!(rejoined, content);
        assert!(
            chunks.len() > 1,
            "the fixture must actually exercise the hard-split path"
        );
    }

    #[test]
    #[should_panic(expected = "chunk_bytes_max must be positive")]
    fn test_text_chunks_zero_bound_panics() {
        let _ = text_chunks("anything", 0);
    }

    #[test]
    fn test_text_chunks_of_a_character_wider_than_the_bound_keeps_it_whole_and_byte_faithful() {
        // '\u{1F600}' is 4 bytes in UTF-8; every bound below that width drives
        // `char_boundary_at_or_before` down to zero, so it must fall back to the character's
        // own width instead of producing an empty or invalid-UTF-8 piece.
        let content = "\u{1F600}";
        for chunk_bytes_max in 1..=3 {
            let chunks = text_chunks(content, chunk_bytes_max);
            assert_eq!(
                contents(&chunks),
                [content],
                "chunk_bytes_max={chunk_bytes_max} must still yield the whole character as one piece"
            );
            assert_eq!(offsets(&chunks), [0]);
            let rejoined: String = chunks.iter().map(TextChunk::content).collect();
            assert_eq!(
                rejoined, content,
                "chunk_bytes_max={chunk_bytes_max} must reproduce the character byte-for-byte"
            );
        }
    }
}
