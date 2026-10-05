//! The warnings section of an answer, one line per read warning.
//!
//! ```text
//! 2 warnings
//!   results_truncated · results_max 100
//!   stale_index · index_tree_revision 3f9a1c2e · captured_tree_revision 3f9a1c2f: index lags the captured tree
//! ```
//!
//! A warning line is `<code>[ · <evidence>]...[: <detail>]`. The evidence is every payload field
//! except `detail`, as `<wire name> <value>`, in declaration order. A hex value of 8 or more
//! characters is cut to 8. An evidence value or a detail that holds ` · ` or `: ` is quoted, with
//! `"` and `\` escaped inside the quotes. Control characters are made visible so one warning
//! stays on one line.

use rift_protocol::read::ReadWarning;

use super::facts::{DETAIL_SEPARATOR, FACT_SEPARATOR, cut_hash, quoted_value};
use super::inline::fields_of;
use super::layout;
use crate::output::text::{TextError, TextWriter, visible};

/// Wire name of the tag field that holds the warning code.
const CODE_FIELD: &str = "code";
/// Wire name of the prose field, written after the evidence.
const DETAIL_FIELD: &str = "detail";
/// Noun counted by the title of the warnings section.
const WARNING_NOUN: &str = "warning";

/// The line of `warning`.
///
/// # Errors
///
/// Fails when the warning holds a shape inline text does not write.
pub(super) fn line(warning: &ReadWarning) -> Result<String, TextError> {
    let mut text = String::new();
    let mut detail = String::new();
    for (index, (key, value)) in fields_of(warning)?.into_iter().enumerate() {
        match (index, key) {
            (0, CODE_FIELD) => text.push_str(&value),
            (_, DETAIL_FIELD) => detail = value,
            (_, key) => {
                text.push_str(FACT_SEPARATOR);
                text.push_str(key);
                text.push(' ');
                text.push_str(&quoted_value(cut_hash(&value)));
            }
        }
    }
    if !detail.trim().is_empty() {
        text.push_str(DETAIL_SEPARATOR);
        text.push_str(&quoted_value(&detail));
    }
    Ok(visible(text.trim_end()).into_owned())
}

/// Writes the warnings section of `warnings`. No warnings write nothing.
///
/// # Errors
///
/// Fails like [`line`], and when `out` refuses the write.
pub(super) fn section(out: &mut TextWriter, warnings: &[ReadWarning]) -> Result<(), TextError> {
    let lines = warnings.iter().map(line).collect::<Result<Vec<_>, _>>()?;
    lines_section(out, &lines)
}

/// Writes a warnings section whose lines are already written out. No lines write nothing.
///
/// # Errors
///
/// Fails when `out` refuses the write.
pub(super) fn lines_section(out: &mut TextWriter, lines: &[String]) -> Result<(), TextError> {
    if lines.is_empty() {
        return Ok(());
    }
    layout::title(out, lines.len(), WARNING_NOUN, None)?;
    for text in lines {
        layout::entry(out, 0, text)?;
    }
    Ok(())
}
