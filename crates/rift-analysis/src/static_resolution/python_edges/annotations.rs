//! Named Python annotations used for static receiver lookup.

use rift_core::ReferenceRole;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::SymbolKind;
use rift_syntax::ByteRange;

use super::super::{File, Index, StaticResolution, Target, Work};
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

    /// Resolves the written types inside compound string annotations.
    pub(in crate::static_resolution) fn python_quoted_type_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        resolution: &mut StaticResolution,
    ) {
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            if reference.role != ReferenceRole::Type {
                continue;
            }
            let range = reference.target;
            let Some(written) = quoted_type_text(file, range) else {
                continue;
            };
            let Some(names) = quoted_type_names(written, work, |head, work| {
                slice_range(range, written, head)
                    .is_some_and(|head| self.python_quoted_generic(file, head, work))
            }) else {
                continue;
            };
            for name in names {
                if !work.charge() {
                    return;
                }
                if let Some(range) = slice_range(range, written, name)
                    && let Some(target) = self.python_type(file, range, work)
                {
                    resolution.record(self.python_access(file, range, ReferenceRole::Type, target));
                }
            }
        }
    }

    fn python_quoted_generic(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> bool {
        let Some(name) = file.text(range) else {
            return false;
        };
        if matches!(
            name,
            "dict" | "frozenset" | "list" | "set" | "tuple" | "type"
        ) {
            return self.external_type(file, range, work)
                .and_then(|target| SymbolIdentity::parse(target.as_str()).ok())
                .is_some_and(|identity| {
                    matches!(identity.owner(), SymbolOwner::Runtime { runtime } if runtime == "cpython")
                        && identity.qualified_path().iter().map(String::as_str).eq(["builtins", name])
                });
        }
        self.python_library_name(file, range, work)
            .is_some_and(|qualified| {
                qualified.rsplit_once('.').is_some_and(|(module, name)| {
                    matches!(module, "typing" | "typing_extensions" | "collections.abc")
                        && matches!(
                            name,
                            "Dict"
                                | "FrozenSet"
                                | "Iterable"
                                | "Iterator"
                                | "List"
                                | "Mapping"
                                | "MutableMapping"
                                | "Optional"
                                | "Sequence"
                                | "Set"
                                | "Tuple"
                                | "Type"
                                | "Union"
                        )
                })
            })
    }
}

/// Longest quoted type text and deepest generic nesting this pass reads.
const QUOTED_TYPE_BYTES_MAX: usize = 4096;
const QUOTED_TYPE_DEPTH_MAX: u8 = 16;

pub(in crate::static_resolution) fn quoted_type_text<'file>(
    file: &'file File<'_>,
    range: ByteRange,
) -> Option<&'file str> {
    let written = file.text(range)?;
    if written.len() > QUOTED_TYPE_BYTES_MAX || !written.contains(['[', '|']) {
        return None;
    }
    let delimited = file.text(ByteRange {
        start: range.start.checked_sub(1)?,
        end: range.end.checked_add(1)?,
    })?;
    let bytes = delimited.as_bytes();
    let first = *bytes.first()?;
    (matches!(first, b'\'' | b'"') && bytes.last() == Some(&first)).then_some(written)
}

fn quoted_type_names<'text>(
    written: &'text str,
    work: &mut Work,
    mut generic: impl FnMut(&str, &mut Work) -> bool,
) -> Option<Vec<&'text str>> {
    let mut depth = 0_u8;
    for byte in written.bytes() {
        match byte {
            b'[' if depth < QUOTED_TYPE_DEPTH_MAX => depth += 1,
            b']' if depth != 0 => depth -= 1,
            b'.' | b',' | b'|' | b'_' | b' ' | b'\t' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {}
            _ => return None,
        }
    }
    if depth != 0 {
        return None;
    }
    let mut pending = vec![written];
    let mut names = Vec::new();
    while let Some(text) = pending.pop() {
        if !work.charge() || text.trim_end().ends_with('|') {
            return None;
        }
        let alternatives = top_level_parts(text, '|', work)?;
        if alternatives.len() > 1 {
            pending.extend(alternatives.into_iter().rev());
            continue;
        }
        let part = alternatives.first().copied()?;
        if let Some((head, args)) = part.split_once('[') {
            let head = head.trim_end();
            let args = top_level_parts(args.strip_suffix(']')?, ',', work)?;
            if !quoted_type_path(head) || args.is_empty() || !generic(head, work) {
                return None;
            }
            names.push(head);
            pending.extend(args.into_iter().rev());
        } else if !matches!(part, "None" | "...") {
            if !quoted_type_path(part) {
                return None;
            }
            names.push(part);
        }
    }
    Some(names)
}

fn quoted_type_path(path: &str) -> bool {
    path.split('.').all(|name| {
        let mut bytes = name.bytes();
        bytes
            .next()
            .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
            && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
    })
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
