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
        let annotation = self.python_stated_annotation(file, annotation, work)?;
        let text = annotation_text(file.text(annotation)?);
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
        // Annotation lookup follows declarations and imports only, so cyclic annotations cannot
        // recurse: a name typed by what it iterates or unpacks names no class.
        let mut value = self.declared_value(file, binding, work)?;
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

    /// The annotation inside every `Optional[...]`, and every `Union` beside `None`, around it.
    ///
    /// Each pass enters a strictly shorter part of the written text.
    pub(super) fn python_stated_annotation(
        &self,
        file: &File<'source>,
        mut annotation: ByteRange,
        work: &mut Work,
    ) -> Option<ByteRange> {
        loop {
            if !work.charge() {
                return None;
            }
            let written = file.text(annotation)?;
            let text = annotation_text(written);
            match self.python_optional_member(file, annotation, written, text, work) {
                Some(inner) => annotation = inner,
                None => return Some(annotation),
            }
        }
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
            let mut members = top_level_parts(inner, ',', work)?
                .into_iter()
                .filter(|member| member.trim_matches(['\'', '"']) != "None");
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

/// A written annotation without the quotes of a string annotation.
pub(super) fn annotation_text(written: &str) -> &str {
    written.trim().trim_matches(['\'', '"'])
}

/// The source range of `slice`, a part of `written`, the text at `range`.
pub(super) fn slice_range(range: ByteRange, written: &str, slice: &str) -> Option<ByteRange> {
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

/// Text one charge of the work budget covers.
const CHARGED_BYTES: usize = 64;

/// The parts of `text` between `separator`s outside brackets, parentheses and quotes, without
/// the spaces around each. A trailing separator adds no part.
pub(super) fn top_level_parts<'text>(
    text: &'text str,
    separator: char,
    work: &mut Work,
) -> Option<Vec<&'text str>> {
    for _ in 0..=text.len() / CHARGED_BYTES {
        if !work.charge() {
            return None;
        }
    }
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut quote = None;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        match (quote, character) {
            (Some(open), _) => {
                if character == open {
                    quote = None;
                }
            }
            (None, '\'' | '"') => quote = Some(character),
            (None, '[' | '(') => depth += 1,
            (None, ']' | ')') => depth = depth.saturating_sub(1),
            (None, _) if depth == 0 && character == separator => {
                parts.push(text[start..index].trim());
                start = index + separator.len_utf8();
            }
            (None, _) => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    Some(parts)
}
