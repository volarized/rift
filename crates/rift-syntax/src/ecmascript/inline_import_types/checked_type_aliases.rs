//! Scoped draft for correcting an already captured TypeScript alias's full item range.

use std::collections::BTreeMap;

use tree_sitter::{Node, Parser};

use crate::{ByteRange, SyntaxLimits, SyntaxSource, SyntaxSymbol, extract, failure::RiftError};

use super::{Candidate, bounded_candidates, checked_owner, comparison_tree};

pub(in crate::ecmascript) fn recover(
    parser: &mut Parser,
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    symbols: &mut [SyntaxSymbol],
) -> Result<(), RiftError> {
    let candidates = bounded_candidates(source, limits, |_| true)?;
    let Some((compared, candidates)) = comparison_tree(parser, root, source, candidates)? else {
        return Ok(());
    };
    let aliases = unique_aliases(symbols);
    for candidate in &candidates {
        let Some(alias) = complete_type_alias(
            root,
            compared.root_node(),
            candidate,
            source.text,
            limits.syntax_depth_max(),
        ) else {
            continue;
        };
        let Some(&position) = aliases.get(&alias.name) else {
            continue;
        };
        let symbol = &mut symbols[position];
        if symbol.item_range != alias.truncated
            || symbol.range.end != alias.truncated.end
            || symbol.body_range.is_some_and(|body| {
                body.start != alias.value.start || body.end != alias.truncated.end
            })
        {
            continue;
        }
        symbol.range.end = alias.item.end;
        symbol.item_range = alias.item;
        symbol.body_range = Some(alias.value);
        symbol.syntax_element_kind = Some("type_alias_declaration");
    }
    Ok(())
}

fn unique_aliases(symbols: &[SyntaxSymbol]) -> BTreeMap<ByteRange, usize> {
    let mut seen = BTreeMap::new();
    for (index, symbol) in symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| symbol.container.is_none())
    {
        let alias = if symbol.kind == "type_alias" {
            symbol.name_range.map(|range| (range, index))
        } else {
            None
        };
        seen.entry(symbol.name.as_str())
            .and_modify(|prior| *prior = None)
            .or_insert(alias);
    }
    seen.into_values().flatten().collect()
}

#[derive(Debug)]
struct Alias {
    name: ByteRange,
    truncated: ByteRange,
    item: ByteRange,
    value: ByteRange,
}

fn complete_type_alias(
    root: Node<'_>,
    compared: Node<'_>,
    candidate: &Candidate,
    text: &str,
    depth_max: usize,
) -> Option<Alias> {
    let prefix = candidate.prefix.start;
    let keyword_end = prefix.checked_add("import".len())?;
    let keyword = root.named_descendant_for_byte_range(prefix, keyword_end)?;
    if keyword.kind() != "type_identifier" || keyword.byte_range() != (prefix..keyword_end) {
        return None;
    }
    let original = keyword.parent()?;
    let name = original.child_by_field_name("name")?;
    if original.kind() != "type_alias_declaration"
        || original.child_by_field_name("value") != Some(keyword)
        || original.child_by_field_name("type_parameters").is_some()
        || original.end_byte() != keyword_end
        || !top_level(original, root)
        || !only_missing_separator(original, keyword_end)
    {
        return None;
    }
    let qualifier =
        compared.named_descendant_for_byte_range(candidate.qualifier, candidate.qualifier + 1)?;
    let (checked, kind) = checked_owner(qualifier, depth_max)?;
    let value = checked.child_by_field_name("value")?;
    if qualifier.kind() != "type_identifier"
        || qualifier.start_byte() != candidate.qualifier
        || kind != "type_alias_declaration"
        || value.start_byte() != qualifier.start_byte()
        || !(value == qualifier || matches!(value.kind(), "intersection_type" | "union_type"))
        || checked.child_by_field_name("name")?.byte_range() != name.byte_range()
        || checked.start_byte() != original.start_byte()
        || !top_level(checked, compared)
        || checked.has_error()
        || text.get(value.end_byte()..checked.end_byte())?.trim() != ";"
    {
        return None;
    }
    let item = extract::byte_range(checked).ok()?;
    Some(Alias {
        name: extract::byte_range(name).ok()?,
        truncated: extract::byte_range(original).ok()?,
        item,
        value: ByteRange {
            start: u64::try_from(prefix).ok()?,
            end: u64::try_from(value.end_byte()).ok()?,
        },
    })
}

fn top_level(item: Node<'_>, root: Node<'_>) -> bool {
    item.parent().is_some_and(|parent| {
        parent == root
            || (parent.kind() == "export_statement"
                && parent.child_by_field_name("declaration") == Some(item)
                && parent.parent() == Some(root))
    })
}

fn only_missing_separator(item: Node<'_>, offset: usize) -> bool {
    let mut cursor = item.walk();
    let mut errors = item
        .children(&mut cursor)
        .filter(|child| child.has_error() || child.is_missing());
    errors.next().is_some_and(|missing| {
        missing.is_missing()
            && missing.kind() == ";"
            && missing.start_byte() == offset
            && missing.end_byte() == offset
    }) && errors.next().is_none()
}
