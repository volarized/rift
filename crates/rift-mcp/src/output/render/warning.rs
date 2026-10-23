//! The warnings section of an answer, with equal framework issues grouped by file.
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
//! Equal framework issues share one heading and a sorted list of distinct files. Other equal
//! warning lines appear once. Structured warning facts retain their original form.

use std::collections::{BTreeMap, BTreeSet};

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
    fields_line(warning, false)
}

fn fields_line(warning: &ReadWarning, omit_unit: bool) -> Result<String, TextError> {
    let mut text = String::new();
    let mut detail = match warning {
        ReadWarning::SymbolNotFound { .. } => "no symbols available with this name".to_owned(),
        _ => String::new(),
    };
    for (index, (key, value)) in fields_of(warning)?.into_iter().enumerate() {
        match (index, key) {
            (0, CODE_FIELD) => text.push_str(&value),
            (_, DETAIL_FIELD) => detail = value,
            (_, "unit") if omit_unit => {}
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
    let mut positions = BTreeMap::new();
    let mut groups: Vec<(String, BTreeSet<String>)> = Vec::new();
    for warning in warnings {
        let file = match warning {
            ReadWarning::FrameworkContextUnresolved { unit, .. } => Some(unit.0.clone()),
            _ => None,
        };
        let head = if file.is_some() {
            fields_line(warning, true)?
        } else {
            line(warning)?
        };
        let index = *positions.entry(head.clone()).or_insert_with(|| {
            groups.push((head, BTreeSet::new()));
            groups.len() - 1
        });
        if let Some(file) = file {
            groups[index].1.insert(file);
        }
    }
    if groups.is_empty() {
        return Ok(());
    }
    layout::title(out, groups.len(), WARNING_NOUN, None)?;
    for (head, files) in groups {
        layout::entry(out, 0, &head)?;
        if !files.is_empty() {
            layout::entry(out, 1, "files:")?;
            for file in files {
                layout::entry(out, 2, &file)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rift_protocol::read::{FileId, ReadWarning, SyntaxFramework};

    use super::section;
    use crate::output::text::{OUTPUT_TEXT_BYTES_MAX, TextWriter};

    #[test]
    fn equal_framework_issues_list_distinct_files_once() {
        let warning = |file: &str, detail: &str| ReadWarning::FrameworkContextUnresolved {
            unit: FileId(format!("rift://file/{file}")),
            framework: SyntaxFramework::Tailwind,
            detail: detail.to_owned(),
        };
        let warnings = [
            warning("b.ts", "missing version"),
            warning("a.ts", "missing version"),
            warning("a.ts", "missing version"),
            warning("c.ts", "missing configuration"),
            ReadWarning::GlobalAccessDisabled,
            ReadWarning::GlobalAccessDisabled,
        ];
        let mut output = TextWriter::new(OUTPUT_TEXT_BYTES_MAX);
        section(&mut output, &warnings).expect("warnings render");
        assert_eq!(
            output.finish().expect("bounded output"),
            concat!(
                "3 warnings\n",
                "\tframework_context_unresolved · framework tailwind: missing version\n",
                "\t\tfiles:\n\t\t\trift://file/a.ts\n\t\t\trift://file/b.ts\n",
                "\tframework_context_unresolved · framework tailwind: missing configuration\n",
                "\t\tfiles:\n\t\t\trift://file/c.ts\n",
                "\tglobal_access_disabled\n",
            )
        );
    }
}
