//! Places typed default trait method heads at checked original macro input names.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ProjectPath, SourceRange};
use rift_protocol::{
    index::PackageSymbol,
    read::{SourceKind, SymbolKind},
};
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxLimits};

use super::{Candidate, ItemKind, Objects, RustLspMacroItems, bytes, parse, range, text_at};

pub(super) fn scopes(original: &SyntaxDocument) -> BTreeSet<ByteRange> {
    original
        .syntax_elements()
        .iter()
        .filter(|row| row.kind == "trait_item" && !row.has_error)
        .map(|row| row.range)
        .collect()
}

pub(super) fn owner<'a>(
    original: &SyntaxDocument,
    invocation: SourceRange,
    scopes: &BTreeSet<ByteRange>,
    existing: &[&'a PackageSymbol],
    objects: &Objects<'_>,
) -> Option<&'a PackageSymbol> {
    let mut candidates = original.facts().symbols().iter().filter(|symbol| {
        symbol.syntax_element_kind == Some("trait_item")
            && scopes.contains(&symbol.item_range)
            && symbol
                .body_range
                .is_some_and(|body| body.encloses(bytes(invocation)))
    });
    let (Some(selected), None) = (candidates.next(), candidates.next()) else {
        return None;
    };
    let mut retained = existing.iter().copied().filter(|row| {
        row.origin.source_kind == SourceKind::Authored
            && row.syntax_element_kind.as_deref() == Some("trait_item")
            && [selected.range, selected.item_range]
                .iter()
                .any(|range| (row.range.start, row.range.end) == (range.start, range.end))
            && objects.get(&row.symbol).is_some_and(|object| {
                object.kind.0 == "trait"
                    && object.name == selected.name
                    && object.kinds.contains(&SymbolKind::MemberContainer)
            })
    });
    let checked = retained.next()?;
    retained.next().is_none().then_some(checked)
}

pub(super) fn candidates(
    path: &ProjectPath,
    source: &str,
    observation: &RustLspMacroItems<'_>,
    labels: &BTreeMap<SourceRange, bool>,
    is_public: bool,
    limits: SyntaxLimits,
) -> BTreeMap<SourceRange, Candidate> {
    let invocation = observation.site.invocation;
    if labels
        .range(invocation..)
        .next()
        .is_none_or(|(position, _)| position.start() >= invocation.end())
    {
        return BTreeMap::new();
    }
    let Some(expansion) = parse(path, observation.expansion, limits) else {
        return BTreeMap::new();
    };
    let emitted = default_methods(&expansion, is_public);
    let mut written = BTreeMap::new();
    if !rift_syntax::tokens(source, bytes(invocation), limits, |token| {
        let Some(position) = range(token) else {
            return false;
        };
        if labels.get(&position) == Some(&true)
            && let Some(name) = text_at(source, position)
            && emitted.contains_key(name)
        {
            written
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(position));
        }
        true
    }) {
        return BTreeMap::new();
    }
    emitted
        .into_iter()
        .filter_map(|(name, method)| Some((written.get(name).copied().flatten()?, method?)))
        .collect()
}

fn default_methods(
    expansion: &SyntaxDocument,
    is_public: bool,
) -> BTreeMap<&str, Option<Candidate>> {
    let syntax = expansion.syntax_elements();
    let direct = syntax
        .iter()
        .filter(|row| row.kind == "function_item" && row.parent == Some(0))
        .map(|row| row.range)
        .collect::<BTreeSet<_>>();
    let mut output = BTreeMap::new();
    for method in expansion.facts().symbols() {
        if method.syntax_element_kind != Some("function_item")
            || !direct.contains(&method.item_range)
            || method.container.is_some()
            || method.body_range.is_none()
            || method.visibility.as_deref() != Some("private")
            || !rift_core::is_portable_name(&method.name)
        {
            continue;
        }
        let candidate = Candidate {
            name: method.name.clone(),
            relative: vec![method.name.clone()],
            kind: ItemKind {
                exact: "function",
                kinds: &[SymbolKind::Value, SymbolKind::Callable],
                child: false,
            },
            visibility: is_public.then(|| "pub".to_owned()),
            variant_owner: None,
            member_owner: None,
        };
        output
            .entry(method.name.as_str())
            .and_modify(|row| *row = None)
            .or_insert(Some(candidate));
    }
    output
}
