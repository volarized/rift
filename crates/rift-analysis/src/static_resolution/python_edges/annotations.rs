//! Named Python annotations used for static receiver lookup.

use rift_protocol::read::SymbolKind;
use rift_syntax::ByteRange;

use super::super::{File, Index, Target, Work};
use super::{Value, edge_python_binding};

impl<'source> Index<'source> {
    pub(super) fn python_annotation(
        &self,
        file: &File<'source>,
        annotation: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if !work.charge() {
            return None;
        }
        let written = file.text(annotation)?;
        let text = written.trim().trim_matches(['\'', '"']);
        if let Some(inner) = self.python_optional_member(file, annotation, written, text, work) {
            return self.python_annotation(file, inner, work);
        }
        let mut alternatives = text
            .split('|')
            .map(str::trim)
            .filter(|name| *name != "None");
        let name = alternatives.next()?;
        if alternatives.next().is_some() {
            return None;
        }
        let name = name.split('[').next()?.trim();
        let mut names = name.split('.').map(str::trim);
        let binding = edge_python_binding(self, file, names.next()?, annotation.start, work)?;
        // Annotation lookup follows declarations and imports only, so cyclic annotations cannot recurse.
        let mut value = self.declaration_value(file, binding, work)?;
        for member in names {
            if !work.charge() {
                return None;
            }
            value = match value {
                Value::Module(name) => {
                    let file = self.files.get(self.edge_python.path(&name)?)?;
                    self.export_value(file, member, work)?
                }
                Value::Symbol(class) => Value::Symbol(self.python_own_member(class, member, work)?),
                Value::Instance(_) | Value::Member { .. } => return None,
            };
        }
        self.value_target(value)
            .filter(|target| target.symbol.kinds.contains(&SymbolKind::Constructible))
    }

    /// The one member of `Optional[X]` or of a `Union` whose other members are `None`.
    fn python_optional_member(
        &self,
        file: &File<'source>,
        annotation: ByteRange,
        written: &str,
        text: &str,
        work: &mut Work,
    ) -> Option<ByteRange> {
        let open = text.find('[')?;
        let inner = text.strip_suffix(']')?.get(open + 1..)?;
        let head = slice_range(annotation, written, text.get(..open)?.trim())?;
        let member = if self.python_typing_name(file, head, "Optional", work) {
            inner
        } else if self.python_typing_name(file, head, "Union", work) {
            let mut members = top_level_members(inner)
                .into_iter()
                .filter(|member| member.trim().trim_matches(['\'', '"']) != "None");
            let member = members.next()?;
            if members.next().is_some() {
                return None;
            }
            member
        } else {
            return None;
        };
        slice_range(annotation, written, member.trim())
    }
}

/// The source range of `slice`, a part of `written`, the text at `range`.
fn slice_range(range: ByteRange, written: &str, slice: &str) -> Option<ByteRange> {
    let offset = (slice.as_ptr() as usize).checked_sub(written.as_ptr() as usize)?;
    if offset.checked_add(slice.len())? > written.len() {
        return None;
    }
    let start = range.start.checked_add(u64::try_from(offset).ok()?)?;
    Some(ByteRange {
        start,
        end: start.checked_add(u64::try_from(slice.len()).ok()?)?,
    })
}

/// Comma separated members of a subscript, outside nested brackets and parentheses.
fn top_level_members(text: &str) -> Vec<&str> {
    let mut members = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        match character {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                members.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    members.push(&text[start..]);
    members
}
