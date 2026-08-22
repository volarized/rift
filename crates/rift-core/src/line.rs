//! Line-ending vocabulary and byte-accurate line arithmetic.
//!
//! Rift's byte-fidelity policy: segmenting source text for reassembly or comparison always uses
//! `split_inclusive('\n')` (or explicit byte slicing), never `str::lines()`. `str::lines()`
//! strips a trailing `\r` from every CRLF line and drops whether the source ended with a
//! newline at all — re-joining its output silently rewrites CRLF to LF and manufactures or
//! removes a trailing newline the input never had or lost. Every site that reconstructs,
//! stores, or diffs source bytes must keep the exact ending each line carried.

/// The line-feed byte terminating an LF line, and the final byte of a CRLF line.
pub const LINE_FEED: u8 = b'\n';

/// The two-byte line ending Windows text and many editors write.
pub const CRLF: &str = "\r\n";

/// Which line ending one line carries, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// Terminated by `\n` alone.
    Lf,
    /// Terminated by `\r\n`.
    CrLf,
}

impl LineEnding {
    /// Returns the ending `line` carries. `\r\n` is checked before `\n` so a CRLF line is never
    /// misreported as LF.
    #[must_use]
    pub fn of(line: &str) -> Option<Self> {
        if line.ends_with(CRLF) {
            Some(Self::CrLf)
        } else if line.ends_with(char::from(LINE_FEED)) {
            Some(Self::Lf)
        } else {
            None
        }
    }

    /// Returns this ending's literal bytes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => CRLF,
        }
    }
}

/// Strips `line`'s ending: `\r\n` first, then `\n`, leaving `line` unchanged when neither
/// matches.
#[must_use]
pub fn without_ending(line: &str) -> &str {
    line.strip_suffix(CRLF)
        .or_else(|| line.strip_suffix(char::from(LINE_FEED)))
        .unwrap_or(line)
}

/// Returns the 1-based line containing `byte_offset` in `source`, clamping an offset past
/// `source`'s end to the end.
///
/// Counting line-feed bytes alone is correct for CRLF as well as LF: a CRLF ending always
/// contains its LF byte, so each line is still counted exactly once.
#[must_use]
#[expect(
    clippy::naive_bytecount,
    reason = "one pass over an already-loaded source per call does not justify the bytecount dependency"
)]
pub fn line_number_at(source: &str, byte_offset: u64) -> u64 {
    let end = usize::try_from(byte_offset)
        .unwrap_or(source.len())
        .min(source.len());
    let lines_before = source.as_bytes()[..end]
        .iter()
        .filter(|&&byte| byte == LINE_FEED)
        .count();
    u64::try_from(lines_before + 1).unwrap_or(u64::MAX)
}

/// Returns the byte offset where 1-based `line` starts in `source`. A `line` at or past the
/// line count falls back to `source`'s length.
#[must_use]
pub fn line_start_offset(source: &str, line: usize) -> u64 {
    let mut current = 1_usize;
    for (index, byte) in source.bytes().enumerate() {
        if current == line {
            return u64::try_from(index).unwrap_or(u64::MAX);
        }
        if byte == LINE_FEED {
            current += 1;
        }
    }
    u64::try_from(source.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{LineEnding, line_number_at, line_start_offset, without_ending};

    #[test]
    fn line_ending_of_reports_lf_and_crlf_and_neither() {
        assert_eq!(LineEnding::of("one\n"), Some(LineEnding::Lf));
        assert_eq!(LineEnding::of("one\r\n"), Some(LineEnding::CrLf));
        assert_eq!(LineEnding::of("one"), None);
        assert_eq!(LineEnding::of(""), None);
    }

    #[test]
    fn line_ending_as_str_round_trips_its_bytes() {
        assert_eq!(LineEnding::Lf.as_str(), "\n");
        assert_eq!(LineEnding::CrLf.as_str(), "\r\n");
    }

    #[test]
    fn without_ending_strips_crlf_before_lf_and_leaves_missing_newline_unchanged() {
        assert_eq!(without_ending("one\r\n"), "one");
        assert_eq!(without_ending("one\n"), "one");
        assert_eq!(without_ending("one"), "one");
        assert_eq!(without_ending(""), "");
    }

    #[test]
    fn line_number_at_counts_lf_lines_including_crlf_and_missing_trailing_newline() {
        let lf = "one\ntwo\nthree\n";
        assert_eq!(line_number_at(lf, 0), 1);
        assert_eq!(line_number_at(lf, 4), 2);
        assert_eq!(line_number_at(lf, 8), 3);

        let crlf = "one\r\ntwo\r\nthree\r\n";
        assert_eq!(line_number_at(crlf, 0), 1);
        assert_eq!(line_number_at(crlf, 5), 2);
        assert_eq!(line_number_at(crlf, 10), 3);

        let missing_trailing_newline = "one\ntwo";
        assert_eq!(
            line_number_at(
                missing_trailing_newline,
                missing_trailing_newline.len() as u64
            ),
            2
        );

        let mixed = "one\r\ntwo\nthree";
        assert_eq!(line_number_at(mixed, mixed.len() as u64), 3);
    }

    #[test]
    fn line_number_at_handles_empty_source_and_offset_beyond_end() {
        assert_eq!(line_number_at("", 0), 1);
        assert_eq!(line_number_at("", 100), 1);
        let source = "one\ntwo\n";
        assert_eq!(
            line_number_at(source, source.len() as u64),
            line_number_at(source, source.len() as u64 + 1_000)
        );
    }

    #[test]
    fn line_start_offset_finds_lf_and_crlf_line_starts() {
        let lf = "one\ntwo\nthree\n";
        assert_eq!(line_start_offset(lf, 1), 0);
        assert_eq!(line_start_offset(lf, 2), 4);
        assert_eq!(line_start_offset(lf, 3), 8);

        let crlf = "one\r\ntwo\r\nthree\r\n";
        assert_eq!(line_start_offset(crlf, 1), 0);
        assert_eq!(line_start_offset(crlf, 2), 5);
        assert_eq!(line_start_offset(crlf, 3), 10);
    }

    #[test]
    fn line_start_offset_beyond_last_line_returns_source_length() {
        let source = "one\ntwo\nthree\n";
        assert_eq!(line_start_offset(source, 100), source.len() as u64);
        assert_eq!(line_start_offset("", 1), 0);
    }

    #[test]
    fn line_start_offset_handles_mixed_endings() {
        let mixed = "one\r\ntwo\nthree";
        assert_eq!(line_start_offset(mixed, 1), 0);
        assert_eq!(line_start_offset(mixed, 2), 5);
        assert_eq!(line_start_offset(mixed, 3), 9);
    }
}
