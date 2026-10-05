//! Byte-limited line sink behind every rendered answer.
//!
//! [`TextWriter`] appends raw lines and refuses to grow past the byte limit given to
//! [`TextWriter::new`]. The first failure is kept and [`TextWriter::finish`] returns it, so a
//! failed write yields no text. [`visible`] makes control characters of a single-line fact
//! readable.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::iter;

use serde::ser;

/// Byte limit of one rendered answer.
pub(crate) const OUTPUT_TEXT_BYTES_MAX: usize = 16 << 20;

/// Ends every line.
const LINE_FEED: char = '\n';
/// Ends every line, as a string slice.
const LINE_END: &str = "\n";
/// Text of one indent level. Every indent this writer adds is a run of this unit; changing the
/// unit changes the indent of every answer. Text an answer copies, such as source, keeps its own
/// whitespace.
pub(crate) const INDENT_UNIT: &str = "\t";

/// Output grew past the byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputOverflow {
    /// Byte limit the output crossed.
    pub(crate) limit: usize,
}

impl fmt::Display for OutputOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "output exceeds the limit of {} bytes",
            self.limit
        )
    }
}

/// Failure of a text write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextError {
    /// Output grew past the byte limit.
    Overflow(OutputOverflow),
    /// The value has a shape compact text does not render.
    Unsupported(&'static str),
    /// A `Serialize` implementation reported its own failure.
    Custom(String),
}

impl fmt::Display for TextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow(overflow) => overflow.fmt(formatter),
            Self::Unsupported(shape) => write!(formatter, "unsupported shape in text: {shape}"),
            Self::Custom(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TextError {}

impl ser::Error for TextError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::Custom(message.to_string())
    }
}

/// The output string, its byte limit, and the first failure.
struct Sink {
    output: String,
    limit: usize,
    failure: Option<TextError>,
}

impl Sink {
    /// Keeps the first failure and returns it.
    fn fail(&mut self, error: TextError) -> TextError {
        self.failure.get_or_insert(error).clone()
    }

    /// Admits `extra` more output bytes, or fails with the overflow.
    fn admit(&mut self, extra: usize) -> Result<(), TextError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let fits = self
            .output
            .len()
            .checked_add(extra)
            .is_some_and(|total| total <= self.limit);
        if fits {
            Ok(())
        } else {
            Err(self.fail(TextError::Overflow(OutputOverflow { limit: self.limit })))
        }
    }

    /// Appends `text` when it fits under the limit.
    fn push(&mut self, text: &str) -> Result<(), TextError> {
        self.admit(text.len())?;
        self.output.push_str(text);
        Ok(())
    }

    /// Appends `levels` indent units when they fit under the limit.
    fn indent(&mut self, levels: usize) -> Result<(), TextError> {
        self.admit(INDENT_UNIT.len().saturating_mul(levels))?;
        self.output.extend(iter::repeat_n(INDENT_UNIT, levels));
        Ok(())
    }
}

/// Collects raw lines under a byte limit.
pub(crate) struct TextWriter {
    sink: Sink,
}

impl TextWriter {
    /// Starts an empty writer that refuses to grow past `bytes_max` bytes.
    pub(crate) fn new(bytes_max: usize) -> Self {
        Self {
            sink: Sink {
                output: String::new(),
                limit: bytes_max,
                failure: None,
            },
        }
    }

    /// Writes `text` as one raw line indented by `indent` levels, each one [`INDENT_UNIT`].
    ///
    /// Empty text writes an empty line with no indent, so no line ends in an indent. `text` holds
    /// no line feed; [`Self::raw_lines`] splits a multi-line text.
    ///
    /// # Errors
    ///
    /// Returns the first failure of this writer: the overflow of the byte limit.
    pub(crate) fn raw_line(&mut self, indent: usize, text: &str) -> Result<(), TextError> {
        self.sink.indent(if text.is_empty() { 0 } else { indent })?;
        self.sink.push(text)?;
        self.sink.push(LINE_END)
    }

    /// Writes each line of `text` as a raw line indented by `indent` levels, bytes unchanged.
    ///
    /// Text with `n` line feeds writes `n + 1` lines, so joining the lines with a line feed after
    /// removing the indent recovers `text` exactly.
    ///
    /// # Errors
    ///
    /// Fails like [`Self::raw_line`].
    pub(crate) fn raw_lines(&mut self, indent: usize, text: &str) -> Result<(), TextError> {
        for line in text.split(LINE_FEED) {
            self.raw_line(indent, line)?;
        }
        Ok(())
    }

    /// Writes one empty line.
    ///
    /// # Errors
    ///
    /// Fails like [`Self::raw_line`].
    pub(crate) fn blank_line(&mut self) -> Result<(), TextError> {
        self.raw_line(0, "")
    }

    /// Returns the text, or the first failure of any write.
    ///
    /// # Errors
    ///
    /// Returns the first failure; no partial text is returned.
    pub(crate) fn finish(self) -> Result<String, TextError> {
        match self.sink.failure {
            Some(failure) => Err(failure),
            None => Ok(self.sink.output),
        }
    }
}

/// Opens and closes a quoted value.
const QUOTE: char = '"';
/// Starts an escape inside a quoted value.
const BACKSLASH: char = '\\';

/// `text` between quotes when it holds one of `delimiters`, else `text` unchanged.
///
/// Inside the quotes each `"` and `\` is written after a `\`, so the closing quote is the first
/// unescaped `"`. A value without a delimiter stays bare, backslash and quote included.
pub(crate) fn quoted<'a>(text: &'a str, delimiters: &[&str]) -> Cow<'a, str> {
    if !delimiters.iter().any(|delimiter| text.contains(delimiter)) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push(QUOTE);
    for character in text.chars() {
        if matches!(character, QUOTE | BACKSLASH) {
            out.push(BACKSLASH);
        }
        out.push(character);
    }
    out.push(QUOTE);
    Cow::Owned(out)
}

/// `text` with each control character written as an escape, so one fact stays on one line.
///
/// The escapes are `\n`, `\r`, `\t`, and `\u{HEX}`. Other characters, backslash and quote
/// included, stay as they are; [`quoted`] escapes those inside a quoted value.
pub(crate) fn visible(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            LINE_FEED => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(other));
            }
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
