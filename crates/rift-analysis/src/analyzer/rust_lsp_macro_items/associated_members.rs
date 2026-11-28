//! Exact authored Rust members inside an independently selected macro invocation.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ProjectPath, SourceRange};
use rift_protocol::{
    index::PackageSymbol,
    read::{SourceKind, SymbolKind},
};
use rift_syntax::{
    ByteRange, RustQuery, RustSyntaxProvider, SyntaxDocument, SyntaxElement, SyntaxLimits,
    SyntaxSource,
};

use super::{Candidate, ItemKind, Objects, bytes, parse_item_source, range, text_at};

const WRAPPER: &str = "impl RiftMacroMemberScope {\n";

pub(super) fn scopes(source: &str, limits: SyntaxLimits) -> BTreeSet<ByteRange> {
    let Some(captures) = RustQuery::new(
        "(impl_item type: (type_identifier) !trait body: (declaration_list)) @owner",
    )
    .and_then(|query| query.captures(source, limits.syntax_elements_max()))
    .ok() else {
        return BTreeSet::new();
    };
    captures.into_iter().map(|capture| capture.range).collect()
}

pub(super) fn owner<'a>(
    original: &SyntaxDocument,
    invocation: SourceRange,
    scopes: &BTreeSet<ByteRange>,
    existing: &[&'a PackageSymbol],
    objects: &Objects<'_>,
) -> Option<&'a PackageSymbol> {
    let implementation = original
        .facts()
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.syntax_element_kind == Some("impl_item")
                && scopes.contains(&symbol.item_range)
                && symbol
                    .body_range
                    .is_some_and(|body| body.encloses(bytes(invocation)))
        })
        .min_by_key(|symbol| symbol.item_range.end - symbol.item_range.start)?;
    let mut retained = existing.iter().copied().filter(|row| {
        row.origin.source_kind == SourceKind::Authored
            && row.syntax_element_kind.as_deref() == Some("impl_item")
            && row.range.start == implementation.range.start
            && row.range.end == implementation.range.end
            && objects.get(&row.symbol).is_some_and(|object| {
                object.kinds.contains(&SymbolKind::MemberContainer)
                    && (nominal(&object.kind.0) || object.kind.0 == "impl_item")
                    && object.name == implementation.name
            })
    });
    let (Some(owner), None) = (retained.next(), retained.next()) else {
        return None;
    };
    Some(owner)
}

pub(super) fn nominal(kind: &str) -> bool {
    matches!(kind, "struct" | "enum" | "union")
}

pub(super) fn candidates(
    path: &ProjectPath,
    source: &str,
    invocation: SourceRange,
    expansion: &str,
    limits: SyntaxLimits,
) -> BTreeMap<SourceRange, Candidate> {
    matches(path, source, invocation, expansion, limits).unwrap_or_default()
}

fn matches(
    path: &ProjectPath,
    source: &str,
    invocation: SourceRange,
    expansion: &str,
    limits: SyntaxLimits,
) -> Option<BTreeMap<SourceRange, Candidate>> {
    let (start, body) = invocation_body(text_at(source, invocation)?)?;
    let start = invocation.start().checked_add(u64::try_from(start).ok()?)?;
    let original = wrapped(body, limits)?;
    let emitted = wrapped(expansion, limits)?;
    let originals = method_heads(path, &original, limits, false)?;
    let generated = method_heads(path, &emitted, limits, true)?;
    let mut output = BTreeMap::new();
    for (name, item) in originals {
        let Some(item) = item else {
            continue;
        };
        let Some(other) = generated.get(&name).and_then(Option::as_ref) else {
            continue;
        };
        let Some(range) = original_range(start, item.name) else {
            continue;
        };
        output.insert(
            range,
            Candidate {
                name: name.clone(),
                relative: vec![name.clone()],
                kind: ItemKind {
                    exact: "function",
                    kinds: &[SymbolKind::Value, SymbolKind::Callable],
                    child: false,
                },
                visibility: other.visibility.clone(),
                variant_owner: None,
                member_owner: None,
            },
        );
        // A declaration of a method can be sound even when its expansion changes a
        // parameter pattern. A parameter needs the complete same-shaped ordered list.
        if item.parameters.keys().ne(other.parameters.keys())
            || item
                .parameters
                .values()
                .map(|parameter| &parameter.name)
                .ne(other.parameters.values().map(|parameter| &parameter.name))
        {
            continue;
        }
        for parameter in item.parameters.into_values() {
            let span = original_range(start, parameter.range)?;
            output.insert(
                span,
                Candidate {
                    name: parameter.name.clone(),
                    relative: vec![name.clone(), format!("{}#parameter0", parameter.name)],
                    kind: ItemKind {
                        exact: "parameter_binding",
                        kinds: &[SymbolKind::Value, SymbolKind::Parameter],
                        child: true,
                    },
                    visibility: None,
                    variant_owner: None,
                    member_owner: Some(range),
                },
            );
        }
    }
    Some(output)
}

fn invocation_body(source: &str) -> Option<(usize, &str)> {
    let (_, tail) = source.split_once('!')?;
    let body = tail.trim_start();
    let start = source.len().checked_sub(body.len())?.checked_add(1)?;
    let closer = match *body.as_bytes().first()? {
        b'(' => ')',
        b'[' => ']',
        b'{' => '}',
        _ => return None,
    };
    Some((start, body.get(1..)?.strip_suffix(closer)?))
}

fn wrapped(body: &str, limits: SyntaxLimits) -> Option<String> {
    let bytes = WRAPPER.len().checked_add(body.len())?.checked_add(2)?;
    if bytes > limits.source_bytes_max() {
        return None;
    }
    Some(format!("{WRAPPER}{body}\n}}"))
}

fn original_range(start: u64, projected: ByteRange) -> Option<SourceRange> {
    let prefix = u64::try_from(WRAPPER.len()).ok()?;
    SourceRange::new(
        start.checked_add(projected.start.checked_sub(prefix)?)?,
        start.checked_add(projected.end.checked_sub(prefix)?)?,
    )
    .ok()
}

struct Method {
    name: ByteRange,
    visibility: Option<String>,
    parameters: BTreeMap<usize, Parameter>,
}

struct Parameter {
    name: String,
    range: ByteRange,
}

fn method_heads(
    path: &ProjectPath,
    text: &str,
    limits: SyntaxLimits,
    generated: bool,
) -> Option<BTreeMap<String, Option<Method>>> {
    let source = SyntaxSource { path, text };
    let document = parse_item_source(source, limits)?;
    let facts = document.facts();
    if !generated && facts.has_errors() {
        return None;
    }
    let elements = document.syntax_elements();
    let mut children = vec![Vec::new(); elements.len()];
    for (index, element) in elements.iter().enumerate() {
        if let Some(parent) = element.parent {
            children.get_mut(parent)?.push(index);
        }
    }
    let invalid = if facts.has_errors() {
        let ranges = elements
            .iter()
            .filter(|element| element.kind == "function_item")
            .map(|element| element.range)
            .collect::<Vec<_>>();
        RustSyntaxProvider::default()
            .incomplete_items(source, limits, &ranges)
            .ok()?
    } else {
        BTreeSet::new()
    };
    let mut methods = BTreeMap::new();
    for (index, element) in elements.iter().enumerate() {
        if (element.kind != "function_item"
            && (generated || element.kind != "function_signature_item"))
            || !direct_wrapper_member(elements, element)
            || element.has_error
            || invalid.contains(&element.range)
        {
            continue;
        }
        let children_here = || children[index].iter().map(|&index| &elements[index]);
        let Some(name) = children_here().find(|element| element.kind == "identifier") else {
            continue;
        };
        let Some(spelling) = range(name.range).and_then(|range| text_at(text, range)) else {
            continue;
        };
        if !rift_core::is_portable_name(spelling) {
            continue;
        }
        let visibility = children_here()
            .find(|element| element.kind == "visibility_modifier")
            .and_then(|element| range(element.range))
            .and_then(|range| text_at(text, range))
            .map(str::to_owned);
        let parameters = method_parameters(text, index, elements, &children);
        let item = Method {
            name: name.range,
            visibility,
            parameters,
        };
        methods
            .entry(spelling.to_owned())
            .and_modify(|candidate| *candidate = None)
            .or_insert(Some(item));
    }
    Some(methods)
}

fn direct_wrapper_member(elements: &[SyntaxElement], function: &SyntaxElement) -> bool {
    let Some(body) = function.parent.and_then(|index| elements.get(index)) else {
        return false;
    };
    body.kind == "declaration_list"
        && body
            .parent
            .and_then(|index| elements.get(index))
            .is_some_and(|owner| owner.kind == "impl_item" && owner.range.start == 0)
}

fn method_parameters(
    text: &str,
    method: usize,
    elements: &[SyntaxElement],
    children: &[Vec<usize>],
) -> BTreeMap<usize, Parameter> {
    let mut parameters = BTreeMap::new();
    let Some(&list) = children[method]
        .iter()
        .find(|&&index| elements[index].kind == "parameters")
    else {
        return parameters;
    };
    for (position, &index) in children[list].iter().enumerate() {
        let Some(name) = parameter_token(&elements[index], &children[index], elements) else {
            continue;
        };
        let Some(spelling) = range(name.range).and_then(|part| text_at(text, part)) else {
            continue;
        };
        if rift_core::is_portable_name(spelling) {
            parameters.insert(
                position,
                Parameter {
                    name: spelling.to_owned(),
                    range: name.range,
                },
            );
        }
    }
    parameters
}

fn parameter_token<'a>(
    parameter: &SyntaxElement,
    children: &[usize],
    elements: &'a [SyntaxElement],
) -> Option<&'a SyntaxElement> {
    let first = elements.get(*children.first()?)?;
    match parameter.kind {
        "self_parameter" => children
            .iter()
            .map(|&index| &elements[index])
            .find(|row| row.kind == "self"),
        "parameter" if matches!(first.kind, "identifier" | "self") => Some(first),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names_require_one_written_head_and_parameter_shape_agreement() {
        let path = ProjectPath::new("src/lib.rs").expect("written member unit");
        let source = "make!(fn show(&self, left: u32, right: u32);)";
        let end = u64::try_from(source.len()).expect("fixture source fits range");
        let invocation = SourceRange::new(0, end).expect("whole written invocation");
        let limits = SyntaxLimits::DEFAULT;
        for (generated, expected) in [
            (
                "pub fn show(&self, left: u32, right: u32) where {}",
                vec!["show", "self", "left", "right"],
            ),
            ("pub fn show(&self, right: u32, left: u32) {}", vec!["show"]),
            (
                "pub fn show(&self, (left, right): (u32, u32)) {}",
                vec!["show"],
            ),
            ("pub fn renamed(&self, left: u32, right: u32) {}", vec![]),
            ("fn show(&self) {} fn show(&self) {}", vec![]),
        ] {
            let observed = candidates(&path, source, invocation, generated, limits)
                .into_keys()
                .filter_map(|range| text_at(source, range))
                .collect::<Vec<_>>();
            assert_eq!(observed, expected, "{generated}");
        }
    }
}
