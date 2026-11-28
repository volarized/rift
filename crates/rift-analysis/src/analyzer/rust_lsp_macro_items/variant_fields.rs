//! Ties an emitted enum variant field to the same written variant's field group.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ProjectPath, SourceRange};
use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxLimits, SyntaxSymbol};

use super::{Candidate, ItemKind, VariantOwner, bytes, parse, range, text_at};

type VariantPath<'a> = (&'a str, &'a str);
type NamedFields<'a> = BTreeMap<&'a str, Option<&'a SyntaxSymbol>>;
type EmittedFields<'a> = BTreeMap<VariantPath<'a>, NamedFields<'a>>;

pub(super) fn extend(
    path: &ProjectPath,
    emitted: &SyntaxDocument,
    invalid: &BTreeSet<ByteRange>,
    source: &str,
    invocation: SourceRange,
    limits: SyntaxLimits,
    candidates: &mut BTreeMap<SourceRange, Candidate>,
) {
    let emitted = emitted_fields(emitted, invalid);
    if emitted.is_empty() {
        return;
    }
    let Some(tokens) = TokenGroups::capture(source, invocation, limits) else {
        return;
    };
    let mut enums = BTreeMap::new();
    let mut variants = BTreeMap::new();
    for (&range, candidate) in candidates.iter() {
        match (candidate.kind.exact, candidate.relative.as_slice()) {
            ("enum", [name]) => {
                enums.insert(name.as_str(), range);
            }
            ("variant", [owner, name]) => {
                variants.insert((owner.as_str(), name.as_str()), range);
            }
            _ => {}
        }
    }
    let mut names = BTreeMap::new();
    let mut group_cache = BTreeMap::new();
    let mut work = limits.syntax_elements_max();
    for ((enum_name, variant_name), fields) in emitted {
        let Some(&enum_token) = enums.get(enum_name) else {
            continue;
        };
        let Some(&variant_token) = variants.get(&(enum_name, variant_name)) else {
            continue;
        };
        let Some(enum_group) = *group_cache
            .entry(enum_token)
            .or_insert_with(|| tokens.enum_body(path, source, enum_token, limits, &mut work))
        else {
            continue;
        };
        let Some(group) = tokens.variant_body(enum_group, variant_token) else {
            continue;
        };
        let authored = authored_fields(path, source, group, limits);
        for (name, field) in fields {
            let (Some(field), Some(Some(written))) = (field, authored.get(name)) else {
                continue;
            };
            let candidate = Candidate {
                name: name.to_owned(),
                relative: vec![
                    enum_name.to_owned(),
                    variant_name.to_owned(),
                    name.to_owned(),
                ],
                kind: ItemKind {
                    exact: "field_declaration",
                    kinds: &[SymbolKind::Value, SymbolKind::Member],
                    child: true,
                },
                visibility: field.visibility.clone(),
                variant_owner: Some(VariantOwner {
                    enumeration: enum_token,
                    name: variant_token,
                }),
                member_owner: None,
            };
            names
                .entry(*written)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(candidate));
        }
    }
    for (range, candidate) in names {
        if let Some(candidate) = candidate {
            candidates.entry(range).or_insert(candidate);
        }
    }
}

fn emitted_fields<'a>(
    emitted: &'a SyntaxDocument,
    invalid: &BTreeSet<ByteRange>,
) -> EmittedFields<'a> {
    let elements = emitted.syntax_elements();
    let groups = elements
        .iter()
        .filter_map(|row| {
            if row.kind != "field_declaration" || row.has_error || invalid.contains(&row.range) {
                return None;
            }
            let mut parent = row.parent;
            for kind in [
                "field_declaration_list",
                "enum_variant",
                "enum_variant_list",
                "enum_item",
            ] {
                let owner = elements.get(parent?)?;
                if owner.kind != kind || owner.has_error || invalid.contains(&owner.range) {
                    return None;
                }
                parent = owner.parent;
            }
            (parent == Some(0)).then_some(row.range)
        })
        .collect::<BTreeSet<_>>();
    let mut fields = EmittedFields::new();
    for field in emitted.facts().symbols() {
        if field.syntax_element_kind != Some("field_declaration")
            || !groups.contains(&field.item_range)
        {
            continue;
        }
        let mut path = field.qualified_name.split("::");
        let Some((owner, variant, name)) = path
            .next()
            .zip(path.next())
            .zip(path.next())
            .map(|((owner, variant), name)| (owner, variant, name))
        else {
            continue;
        };
        if name != field.name || path.next().is_some() {
            continue;
        }
        fields
            .entry((owner, variant))
            .or_default()
            .entry(name)
            .and_modify(|entry| *entry = None)
            .or_insert(Some(field));
    }
    fields
}

fn authored_fields(
    path: &ProjectPath,
    source: &str,
    group: SourceRange,
    limits: SyntaxLimits,
) -> BTreeMap<String, Option<SourceRange>> {
    const HEADER: &str = "struct VariantFields ";
    let Some(text) = text_at(source, group) else {
        return BTreeMap::new();
    };
    // The group's braces came from the original tokens. The prefix only lets the Rust
    // parser check that the contents are field declarations.
    let probe = format!("{HEADER}{text}");
    let Some(document) = parse(path, &probe, limits) else {
        return BTreeMap::new();
    };
    let header = u64::try_from(HEADER.len()).unwrap_or(u64::MAX);
    let mut fields = BTreeMap::new();
    for field in document.facts().symbols() {
        if field.syntax_element_kind != Some("field_declaration")
            || field.container.as_deref() != Some("VariantFields")
        {
            continue;
        }
        let Some(name) = field.name_range.and_then(|name| {
            let start = group.start().checked_add(name.start.checked_sub(header)?)?;
            let end = group.start().checked_add(name.end.checked_sub(header)?)?;
            SourceRange::new(start, end).ok()
        }) else {
            continue;
        };
        if name.end() <= group.end() && text_at(source, name) == Some(field.name.as_str()) {
            fields
                .entry(field.name.clone())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(name));
        }
    }
    fields
}

struct Token<'source> {
    text: &'source str,
    range: SourceRange,
    parent: Option<usize>,
    close: Option<usize>,
}

struct TokenGroups<'source>(Vec<Token<'source>>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delimiter {
    Brace,
    Parenthesis,
    Bracket,
}

enum GroupToken {
    Open(Delimiter),
    Close(Delimiter),
    Other,
}

impl GroupToken {
    fn classify(text: &str) -> Self {
        match text {
            "{" => Self::Open(Delimiter::Brace),
            "(" => Self::Open(Delimiter::Parenthesis),
            "[" => Self::Open(Delimiter::Bracket),
            "}" => Self::Close(Delimiter::Brace),
            ")" => Self::Close(Delimiter::Parenthesis),
            "]" => Self::Close(Delimiter::Bracket),
            _ => Self::Other,
        }
    }
}

impl<'source> TokenGroups<'source> {
    fn capture(
        source: &'source str,
        invocation: SourceRange,
        limits: SyntaxLimits,
    ) -> Option<Self> {
        let mut rows: Vec<Token<'_>> = Vec::new();
        let mut open = Vec::<(usize, Delimiter)>::new();
        let valid = rift_syntax::tokens(source, bytes(invocation), limits, |value| {
            let Some((range, text)) =
                range(value).and_then(|range| text_at(source, range).map(|text| (range, text)))
            else {
                return false;
            };
            let index = rows.len();
            let parent = open.last().map(|&(index, _)| index);
            match GroupToken::classify(text) {
                GroupToken::Open(delimiter) => open.push((index, delimiter)),
                GroupToken::Close(delimiter) => {
                    let Some((opener, expected)) = open.pop() else {
                        return false;
                    };
                    if delimiter != expected {
                        return false;
                    }
                    rows[opener].close = Some(index);
                }
                GroupToken::Other => {}
            }
            rows.push(Token {
                text,
                range,
                parent,
                close: None,
            });
            true
        });
        (valid && open.is_empty()).then_some(Self(rows))
    }

    fn index(&self, token: SourceRange) -> Option<usize> {
        self.0.binary_search_by_key(&token, |row| row.range).ok()
    }

    fn enum_body(
        &self,
        path: &ProjectPath,
        source: &str,
        token: SourceRange,
        limits: SyntaxLimits,
        work: &mut usize,
    ) -> Option<usize> {
        let name = self.index(token)?;
        let parent = self.0.get(name)?.parent?;
        if self.0.get(parent)?.parent.is_some() {
            return None;
        }
        let stop = self.0.get(parent)?.close?;
        let start = name.checked_add(1)?;
        match self.0.get(start)?.text {
            "{" => return Some(start),
            "<" => {}
            _ => return None,
        }
        for (i, candidate) in self.0.iter().enumerate().take(stop).skip(start + 1) {
            *work = work.checked_sub(1)?;
            if candidate.parent != Some(parent) || candidate.text != "{" {
                continue;
            }
            let header = SourceRange::new(token.start(), candidate.range.start()).ok()?;
            let probe = format!("enum {}{{}}", text_at(source, header)?);
            return parse(path, &probe, limits).map(|_| i);
        }
        None
    }

    fn variant_body(&self, enum_body: usize, token: SourceRange) -> Option<SourceRange> {
        let name = self.index(token)?;
        if self.0.get(name)?.parent != Some(enum_body) {
            return None;
        }
        let open = self.0.get(name.checked_add(1)?)?;
        if open.text != "{" || open.parent != Some(enum_body) {
            return None;
        }
        let close = self.0.get(open.close?)?;
        SourceRange::new(open.range.start(), close.range.end()).ok()
    }
}
