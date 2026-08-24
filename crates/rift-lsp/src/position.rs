//! Conversion between LSP positions and byte offsets in one document.
//!
//! An LSP position counts lines, then characters in the negotiated
//! encoding. The index walks at most one line per conversion, so one call
//! costs the bytes of the addressed line plus a binary search over line
//! starts. A position past a line's end or the document's end is refused,
//! never clamped: a clamp would silently move an engine's edit.

use lsp_types::Position;
use rift_core::line::{LineEnding, lines_inclusive, without_ending};
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use serde::Serialize;

use crate::capabilities::PositionEncoding;

/// A position or offset outside the document it addresses.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionFault {
    /// The line number is at or past the document's line count.
    LineOutOfRange {
        /// The line as addressed.
        line: u32,
        /// Lines the document has.
        line_count: u32,
    },
    /// The character is past the addressed line's end.
    CharacterOutOfRange {
        /// The line as addressed.
        line: u32,
        /// The character as addressed.
        character: u32,
        /// Code units the line holds before its ending.
        line_units: u32,
    },
    /// The character lands inside one character's code units.
    CharacterMisaligned {
        /// The line as addressed.
        line: u32,
        /// The character as addressed.
        character: u32,
    },
    /// The byte offset is past the document's end.
    OffsetOutOfRange {
        /// The offset as addressed.
        byte_offset: usize,
        /// Bytes the document holds.
        document_bytes: usize,
    },
    /// The byte offset lands inside one character's UTF-8 bytes.
    OffsetMisaligned {
        /// The offset as addressed.
        byte_offset: usize,
    },
    /// The byte offset lands inside one line's ending bytes.
    OffsetInsideLineEnding {
        /// The offset as addressed.
        byte_offset: usize,
    },
}

impl Fault for PositionFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InternalError)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![ErrorContext::new("fault", fault_label(self))]
    }
}

/// A position outside the document it addresses.
pub type PositionError = Error<PositionFault>;

/// Line starts of one document, for position conversion.
///
/// The index borrows the document text; both sides of a conversion see the
/// same bytes by construction.
#[derive(Debug)]
pub struct LineIndex<'text> {
    text: &'text str,
    line_starts: Vec<usize>,
}

impl<'text> LineIndex<'text> {
    /// Indexes one document's line starts in one pass over its bytes.
    ///
    /// A document ending in a newline gets one final empty line, so the
    /// position just past a trailing newline stays addressable.
    #[must_use]
    pub fn new(text: &'text str) -> Self {
        let mut line_starts = vec![0];
        let mut offset = 0;
        for line in lines_inclusive(text) {
            offset += line.len();
            if LineEnding::of(line).is_some() {
                line_starts.push(offset);
            }
        }
        Self { text, line_starts }
    }

    /// Lines the document has, the empty line after a trailing newline included.
    #[must_use]
    pub fn line_count(&self) -> u32 {
        u32::try_from(self.line_starts.len()).unwrap_or(u32::MAX)
    }

    /// The byte offset one position addresses.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError`] when the line or character is past the
    /// document, or the character splits one character's code units.
    pub fn byte_offset(
        &self,
        encoding: PositionEncoding,
        position: Position,
    ) -> Result<usize, PositionError> {
        let line_start = self.line_start(position.line)?;
        let content = without_ending(self.line_text(position.line as usize, line_start));
        let mut units: u32 = 0;
        for (offset, character) in content.char_indices() {
            if units == position.character {
                return Ok(line_start + offset);
            }
            if units > position.character {
                return Err(Error::new(PositionFault::CharacterMisaligned {
                    line: position.line,
                    character: position.character,
                }));
            }
            units += unit_width(encoding, character);
        }
        if units == position.character {
            return Ok(line_start + content.len());
        }
        Err(Error::new(PositionFault::CharacterOutOfRange {
            line: position.line,
            character: position.character,
            line_units: units,
        }))
    }

    /// The position one byte offset addresses.
    ///
    /// # Errors
    ///
    /// Returns [`PositionError`] when the offset is past the document or
    /// splits one character's UTF-8 bytes.
    pub fn position(
        &self,
        encoding: PositionEncoding,
        byte_offset: usize,
    ) -> Result<Position, PositionError> {
        if byte_offset > self.text.len() {
            return Err(Error::new(PositionFault::OffsetOutOfRange {
                byte_offset,
                document_bytes: self.text.len(),
            }));
        }
        if !self.text.is_char_boundary(byte_offset) {
            return Err(Error::new(PositionFault::OffsetMisaligned { byte_offset }));
        }
        let line_index = self
            .line_starts
            .partition_point(|start| *start <= byte_offset)
            - 1;
        let line_start = self.line_starts[line_index];
        let content = without_ending(self.line_text(line_index, line_start));
        if byte_offset > line_start + content.len() {
            return Err(Error::new(PositionFault::OffsetInsideLineEnding {
                byte_offset,
            }));
        }
        let units: u32 = self.text[line_start..byte_offset]
            .chars()
            .map(|character| unit_width(encoding, character))
            .sum();
        Ok(Position {
            line: u32::try_from(line_index).unwrap_or(u32::MAX),
            character: units,
        })
    }

    /// The byte offset where one addressed line starts.
    fn line_start(&self, line: u32) -> Result<usize, PositionError> {
        self.line_starts.get(line as usize).copied().ok_or_else(|| {
            Error::new(PositionFault::LineOutOfRange {
                line,
                line_count: self.line_count(),
            })
        })
    }

    /// The addressed line's text, ending included.
    fn line_text(&self, line: usize, line_start: usize) -> &str {
        let line_end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        &self.text[line_start..line_end]
    }
}

/// How many `character` units one character occupies in `encoding`.
fn unit_width(encoding: PositionEncoding, character: char) -> u32 {
    let width = match encoding {
        PositionEncoding::Utf8 => character.len_utf8(),
        PositionEncoding::Utf16 => character.len_utf16(),
    };
    u32::try_from(width).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    /// Fixture documents for the newline and multibyte matrix.
    ///
    /// LF and CRLF forms, no trailing newline, with a 2-byte `é`, a
    /// 3-byte `€`, and an astral 4-byte `𝄞` (one surrogate pair).
    const LF: &str = "fn a() {}\nlet é = \"€𝄞\";\nend";
    const CRLF: &str = "fn a() {}\r\nlet é = \"€𝄞\";\r\nend";

    #[test]
    fn line_counts_cover_lf_crlf_and_trailing_newline_forms() {
        assert_eq!(LineIndex::new(LF).line_count(), 3);
        assert_eq!(LineIndex::new(CRLF).line_count(), 3);
        assert_eq!(LineIndex::new("one\ntwo\n").line_count(), 3);
        assert_eq!(LineIndex::new("").line_count(), 1);
    }

    #[test]
    fn round_trips_hold_for_every_boundary_in_both_encodings() {
        for text in [LF, CRLF] {
            let index = LineIndex::new(text);
            for encoding in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
                for (offset, _) in text.char_indices() {
                    let inside_ending =
                        text[..offset].ends_with('\r') && text[offset..].starts_with('\n');
                    if inside_ending {
                        let refused = index
                            .position(encoding, offset)
                            .expect_err("an offset inside CRLF is not addressable");
                        assert_eq!(
                            *refused.fault(),
                            PositionFault::OffsetInsideLineEnding {
                                byte_offset: offset
                            }
                        );
                        continue;
                    }
                    let position = index.position(encoding, offset).expect("in range");
                    assert_eq!(
                        index.byte_offset(encoding, position),
                        Ok(offset),
                        "{text:?}"
                    );
                }
                let end = index.position(encoding, text.len()).expect("document end");
                assert_eq!(index.byte_offset(encoding, end), Ok(text.len()));
            }
        }
    }

    #[test]
    fn utf16_counts_the_astral_character_as_a_surrogate_pair() {
        let index = LineIndex::new(LF);
        let after_astral = LF.rfind('"').expect("closing quote");
        let position = index
            .position(PositionEncoding::Utf16, after_astral)
            .expect("in range");
        assert_eq!(position, at(1, 12));
        assert_eq!(
            index
                .position(PositionEncoding::Utf8, after_astral)
                .expect("in range"),
            at(1, 17)
        );
    }

    #[test]
    fn position_past_the_line_end_is_refused_not_clamped() {
        let index = LineIndex::new(LF);
        let error = index
            .byte_offset(PositionEncoding::Utf16, at(0, 10))
            .expect_err("line 0 holds 9 units");
        assert_eq!(
            *error.fault(),
            PositionFault::CharacterOutOfRange {
                line: 0,
                character: 10,
                line_units: 9
            }
        );
        assert!(index.byte_offset(PositionEncoding::Utf16, at(0, 9)).is_ok());
    }

    #[test]
    fn position_past_the_document_end_is_refused() {
        let index = LineIndex::new("one\ntwo");
        let error = index
            .byte_offset(PositionEncoding::Utf16, at(2, 0))
            .expect_err("two lines exist");
        assert_eq!(
            *error.fault(),
            PositionFault::LineOutOfRange {
                line: 2,
                line_count: 2
            }
        );
        let trailing = LineIndex::new("one\n");
        assert_eq!(
            trailing.byte_offset(PositionEncoding::Utf16, at(1, 0)),
            Ok(4)
        );
    }

    #[test]
    fn characters_inside_a_surrogate_pair_or_multibyte_sequence_are_refused() {
        let index = LineIndex::new(LF);
        let inside_pair = index
            .byte_offset(PositionEncoding::Utf16, at(1, 11))
            .expect_err("unit 11 splits the astral pair");
        assert_eq!(
            *inside_pair.fault(),
            PositionFault::CharacterMisaligned {
                line: 1,
                character: 11
            }
        );
        let inside_euro = index
            .byte_offset(PositionEncoding::Utf8, at(1, 11))
            .expect_err("byte 11 splits the euro sign");
        assert!(matches!(
            inside_euro.fault(),
            PositionFault::CharacterMisaligned { .. }
        ));
    }

    #[test]
    fn offsets_past_the_end_or_inside_a_character_are_refused() {
        let index = LineIndex::new(LF);
        let past = index
            .position(PositionEncoding::Utf16, LF.len() + 1)
            .expect_err("past the document");
        assert_eq!(
            *past.fault(),
            PositionFault::OffsetOutOfRange {
                byte_offset: LF.len() + 1,
                document_bytes: LF.len()
            }
        );
        let inside = LF.find('é').expect("é exists") + 1;
        let misaligned = index
            .position(PositionEncoding::Utf16, inside)
            .expect_err("inside the two-byte character");
        assert_eq!(
            *misaligned.fault(),
            PositionFault::OffsetMisaligned {
                byte_offset: inside
            }
        );
        assert_eq!(misaligned.name(), ErrorName::Wire(ErrorCode::InternalError));
        assert!(misaligned.to_string().contains("offset_misaligned"));
    }
}
