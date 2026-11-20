//! Types written in JavaScript documentation comments.
//!
//! A `/** ... */` comment written right before a function states the types of the
//! function's parameters (`@param {Type} name`) and of its result (`@returns {Type}`). A
//! top-level `@typedef {import("./module").Name} Alias` names a type another module
//! exports. Only a single named type is read: a union with `null` or `undefined`, an
//! optional mark and parentheses around it are allowed; arrays, generics, object types
//! and every other form type nothing. TypeScript reads annotations instead, so the rule
//! holds for the JavaScript grammar only.

use rift_error::RiftError;
use tree_sitter::Node;

use super::super::{StaticBindingKind, StaticDeclaredReturn, StaticFactsParts};
use super::{Kinds, byte_range};
use crate::ByteRange;
use crate::extract::Visited;

/// Maximum tags read from one documentation comment.
const TAGS_MAX: usize = 64;

/// The documentation comment last seen, waiting for the function written after it.
#[derive(Debug, Default)]
pub(in crate::static_analysis) struct Documentation {
    pending: Option<ByteRange>,
}

/// One tag that types a value: the tag's kind, the named type and the written name.
struct Tag<'text> {
    kind: &'text str,
    written: Option<ByteRange>,
    name: Option<&'text str>,
}

impl Kinds {
    /// Records a documentation comment: its type imports now, its parameter and result
    /// types when the function written after it is visited.
    pub(in crate::static_analysis) fn record_comment(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        documentation: &mut Documentation,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let comment = visited.syntax_element();
        if self.type_shapes.is_some() || comment.kind() != "comment" {
            return Ok(());
        }
        let range = byte_range(comment)?;
        let Some(text) = range_text(source, range).filter(|text| text.starts_with("/**")) else {
            return Ok(());
        };
        documentation.pending = Some(range);
        // A type import declared at the top level is visible in the whole module.
        let Some(program) = visited
            .parent()
            .map(Visited::syntax_element)
            .filter(|parent| parent.kind_id() == self.scopes[0])
        else {
            return Ok(());
        };
        for tag in tags(text, range.start).filter(|tag| tag.kind == "typedef") {
            let Some((module, export)) =
                tag.written.and_then(|written| type_import(source, written))
            else {
                continue;
            };
            let Some(alias) = tag
                .name
                .and_then(|name| name_range(source, range, name, written_end(&tag)))
            else {
                continue;
            };
            facts.bindings.push(super::super::StaticBinding {
                name: alias,
                scope: byte_range(program)?,
                available_from: byte_range(program)?.start,
                kind: StaticBindingKind::JavaScriptTypeImport { module, export },
            });
        }
        Ok(())
    }

    /// Types the parameters and the result of a function from the documentation comment
    /// written right before it. Only spaces, names, `.`, `=` and `:` may stand between the
    /// comment and the function, as in `exports.name = function` or `key: function`.
    pub(in crate::static_analysis) fn record_documented_function(
        &self,
        element: Node<'_>,
        source: &str,
        documentation: &mut Documentation,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let function = element.kind_id() == self.arrow
            || element.kind_id() == self.function_scopes[6]
            || self.function.contains(&element.kind_id())
            || self.named_expression[..2].contains(&element.kind_id());
        if !function || self.type_shapes.is_some() {
            return Ok(());
        }
        let Some(comment) = documentation.pending.take() else {
            return Ok(());
        };
        let element_range = byte_range(element)?;
        let gap = usize::try_from(comment.end)
            .ok()
            .zip(usize::try_from(element_range.start).ok())
            .and_then(|(start, end)| source.get(start..end));
        let adjacent = gap.is_some_and(|gap| {
            gap.chars().all(|character| {
                character.is_whitespace()
                    || character.is_alphanumeric()
                    || matches!(character, '_' | '$' | '.' | '=' | ':')
            }) && !gap.contains("=>")
        });
        let Some(text) = range_text(source, comment).filter(|_| adjacent) else {
            return Ok(());
        };
        let parameters = self.documented_parameters(element, source);
        let tags = tags(text, comment.start).collect::<Vec<_>>();
        for (name, range) in &parameters {
            let mut written = tags.iter().filter(|tag| {
                matches!(tag.kind, "param" | "arg" | "argument") && tag.name == Some(*name)
            });
            if let (Some(tag), None) = (written.next(), written.next())
                && let Some(written) = tag.written.and_then(|written| named_type(source, written))
            {
                facts.declared_types.push((*range, written));
            }
        }
        let mut returns = tags
            .iter()
            .filter(|tag| matches!(tag.kind, "returns" | "return"));
        if let (Some(tag), None) = (returns.next(), returns.next())
            && let Some(written) = tag.written.and_then(|written| named_type(source, written))
            && let Some(owner) = self.function_owner(element)?
        {
            facts.return_types.push((element_range, written));
            facts.declared_returns.push(StaticDeclaredReturn {
                owner,
                function: element_range,
                member: None,
                annotation: written,
            });
        }
        Ok(())
    }

    /// The simple parameters of a function: a name, or a name with a default value.
    fn documented_parameters<'text>(
        &self,
        element: Node<'_>,
        source: &'text str,
    ) -> Vec<(&'text str, ByteRange)> {
        let mut names = Vec::new();
        if let Some(single) = element
            .child_by_field_id(self.parameter)
            .filter(|parameter| parameter.kind_id() == self.identifier)
        {
            names.extend(super::text(single, source).zip(byte_range(single).ok()));
            return names;
        }
        let mut cursor = element.walk();
        let Some(list) = element
            .named_children(&mut cursor)
            .find(|child| child.kind_id() == self.parameters)
        else {
            return names;
        };
        let mut cursor = list.walk();
        for parameter in list.named_children(&mut cursor) {
            let name = if parameter.kind_id() == self.identifier {
                Some(parameter)
            } else if self.default_patterns.contains(&parameter.kind_id()) {
                parameter
                    .child_by_field_id(self.left)
                    .filter(|name| name.kind_id() == self.identifier)
            } else {
                None
            };
            if let Some(name) = name {
                names.extend(super::text(name, source).zip(byte_range(name).ok()));
            }
        }
        names
    }
}

fn range_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

fn written_end(tag: &Tag<'_>) -> u64 {
    tag.written.map_or(0, |written| written.end)
}

/// The range of `name` written after `after` inside the comment.
fn name_range(source: &str, comment: ByteRange, name: &str, after: u64) -> Option<ByteRange> {
    let from = usize::try_from(after.max(comment.start)).ok()?;
    let end = usize::try_from(comment.end).ok()?;
    let offset = source.get(from..end)?.find(name)?;
    let start = u64::try_from(from + offset).ok()?;
    Some(ByteRange {
        start,
        end: start.checked_add(u64::try_from(name.len()).ok()?)?,
    })
}

/// The tags of a comment with a type: `@tag {written} name`. `written` is the range inside
/// the braces; `name` is the next word, without the brackets of an optional parameter and
/// its default value.
fn tags(text: &str, start: u64) -> impl Iterator<Item = Tag<'_>> {
    text.match_indices('@')
        .take(TAGS_MAX)
        .filter_map(move |(at, _)| {
            let rest = &text[at + 1..];
            let tag_length = rest
                .find(|character: char| !character.is_ascii_alphabetic())
                .unwrap_or(rest.len());
            let tag = &rest[..tag_length];
            let after_tag = &rest[tag_length..];
            let open = after_tag.len() - after_tag.trim_start().len();
            if !after_tag[open..].starts_with('{') {
                return None;
            }
            let body_start = at + 1 + tag_length + open + 1;
            let mut depth = 1_u32;
            let mut close = None;
            for (offset, character) in text[body_start..].char_indices() {
                match character {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(body_start + offset);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let close = close?;
            let written = Some(ByteRange {
                start: start.checked_add(u64::try_from(body_start).ok()?)?,
                end: start.checked_add(u64::try_from(close).ok()?)?,
            });
            let after = text[close + 1..].trim_start();
            let name = if let Some(optional) = after.strip_prefix('[') {
                let inner = &optional[..optional.find(']')?];
                inner.split('=').next().map(str::trim)
            } else {
                let length = after
                    .find(|character: char| {
                        !(character.is_alphanumeric() || "_$.".contains(character))
                    })
                    .unwrap_or(after.len());
                Some(&after[..length])
            }
            .filter(|name| !name.is_empty() && !name.contains('.'));
            Some(Tag {
                kind: tag,
                written,
                name,
            })
        })
}

/// The single named type a written type states: one name, possibly in parentheses, with
/// `?`, `!` or `=` marks and union members `null` and `undefined`.
fn named_type(source: &str, written: ByteRange) -> Option<ByteRange> {
    let text = range_text(source, written)?;
    let mut named = None;
    let mut offset = 0_usize;
    for member in text.split('|') {
        let trimmed = member.trim_matches(|character: char| {
            character.is_whitespace() || "()?!=".contains(character)
        });
        let at = offset + member.find(trimmed)?;
        offset += member.len() + 1;
        if matches!(trimmed, "null" | "undefined") {
            continue;
        }
        let identifier = trimmed.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character == '$'
                || character.is_alphabetic()
                || (index > 0 && character.is_alphanumeric())
        });
        if named.is_some() || trimmed.is_empty() || !identifier {
            return None;
        }
        let start = written.start.checked_add(u64::try_from(at).ok()?)?;
        named = Some(ByteRange {
            start,
            end: start.checked_add(u64::try_from(trimmed.len()).ok()?)?,
        });
    }
    named
}

/// The unquoted module and the export a written `import("module").Name` type names.
fn type_import(source: &str, written: ByteRange) -> Option<(ByteRange, ByteRange)> {
    let text = range_text(source, written)?;
    let leading = text.len() - text.trim_start().len();
    let rest = text[leading..].strip_prefix("import")?;
    let open = rest.find('(')?;
    if !rest[..open].trim().is_empty() {
        return None;
    }
    let inner = &rest[open + 1..];
    let quoted = inner.trim_start();
    let quote = quoted
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))?;
    let module_length = quoted[1..].find(quote)?;
    let module_at = leading + "import".len() + open + 1 + (inner.len() - quoted.len()) + 1;
    let after = &quoted[1 + module_length + 1..];
    let close = after.find(')')?;
    if !after[..close].trim().is_empty() {
        return None;
    }
    let export = after[close + 1..].trim_end().strip_prefix('.')?;
    let identifier = !export.is_empty()
        && export
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$'));
    if !identifier {
        return None;
    }
    let export_end = text.trim_end().len();
    let base = written.start;
    let module = ByteRange {
        start: base.checked_add(u64::try_from(module_at).ok()?)?,
        end: base.checked_add(u64::try_from(module_at + module_length).ok()?)?,
    };
    let export = ByteRange {
        start: base.checked_add(u64::try_from(export_end - export.len()).ok()?)?,
        end: base.checked_add(u64::try_from(export_end).ok()?)?,
    };
    Some((module, export))
}
