//! Checks written native constant names against their selected Rust expansion.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ProjectPath, SourceRange};
use rift_protocol::{
    identity::SymbolIdentity,
    index::PackageSymbol,
    read::{SourceKind, SymbolKind},
};
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxElement, SyntaxLimits};

use super::{
    Candidate, DeclarationTokens, ItemKind, Objects, RustLspMacroItems, associated_members, bytes,
    item_identity, parse, range, text_at,
};

pub(super) type Labels = BTreeMap<SourceRange, Option<bool>>;

pub(super) fn labels(tokens: DeclarationTokens<'_>, limits: SyntaxLimits) -> Labels {
    let mut rows = BTreeMap::new();
    if tokens.len() <= limits.syntax_elements_max() {
        for &(range, associated) in tokens {
            rows.entry(range)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(associated));
        }
    }
    rows
}

pub(super) struct Owners<'scope, 'graph> {
    pub(super) module: &'scope SymbolIdentity,
    pub(super) existing: &'scope [&'graph PackageSymbol],
    pub(super) objects: &'scope Objects<'graph>,
}

pub(super) fn candidates(
    path: &ProjectPath,
    source: &str,
    observation: &RustLspMacroItems<'_>,
    labels: &Labels,
    owners: &Owners<'_, '_>,
    limits: SyntaxLimits,
) -> BTreeMap<SourceRange, Candidate> {
    let invocation = observation.site.invocation;
    let Some((first, _)) = labels.range(invocation..).next() else {
        return BTreeMap::new();
    };
    if first.start() >= invocation.end() {
        return BTreeMap::new();
    }
    let Some(expansion) = parse(path, observation.expansion, limits) else {
        return BTreeMap::new();
    };
    let emitted = emitted_constants(&expansion, observation.expansion, limits);
    let mut written = BTreeMap::new();
    if !rift_syntax::tokens(source, bytes(invocation), limits, |token| {
        let Some(position) = range(token) else {
            return false;
        };
        if let (Some(Some(associated)), Some(name)) =
            (labels.get(&position), text_at(source, position))
            && emitted.contains_key(&(*associated, name))
        {
            written
                .entry((*associated, name))
                .and_modify(|entry| *entry = None)
                .or_insert(Some(position));
        }
        true
    }) {
        return BTreeMap::new();
    }
    emitted
        .into_iter()
        .filter_map(|(key, candidate)| {
            let position = written.get(&key).copied().flatten()?;
            let candidate = candidate?;
            if key.0 && !owners.retains_nominal(&candidate.relative, invocation) {
                return None;
            }
            Some((position, candidate))
        })
        .collect()
}

impl Owners<'_, '_> {
    fn retains_nominal(&self, relative: &[String], invocation: SourceRange) -> bool {
        let [owner, _] = relative else {
            return false;
        };
        let Some((symbol, _)) = item_identity(self.module, std::slice::from_ref(owner)) else {
            return false;
        };
        if !self.objects.get(&symbol).is_some_and(|object| {
            associated_members::nominal(&object.kind.0) && object.name == *owner
        }) {
            return false;
        }
        let mut retained = self.existing.iter().filter(|row| {
            row.symbol == symbol
                && row.origin.source_kind == SourceKind::Authored
                && matches!(
                    row.syntax_element_kind.as_deref(),
                    Some("struct_item" | "enum_item" | "union_item")
                )
                && (row.range.end <= invocation.start() || row.range.start >= invocation.end())
        });
        retained.next().is_some() && retained.next().is_none()
    }
}

fn emitted_constants<'source>(
    expansion: &'source SyntaxDocument,
    text: &str,
    limits: SyntaxLimits,
) -> BTreeMap<(bool, &'source str), Option<Candidate>> {
    let elements = expansion.syntax_elements();
    let associated = associated_members::scopes(text, limits);
    let placement = elements
        .iter()
        .filter_map(|element| Some((element.range, placement(elements, element, &associated)?)))
        .collect::<BTreeMap<_, _>>();
    let mut output = BTreeMap::new();
    for declaration in expansion.facts().symbols() {
        if declaration.kind != "constant" || declaration.syntax_element_kind != Some("const_item") {
            continue;
        }
        let Some(&associated) = placement.get(&declaration.item_range) else {
            continue;
        };
        let relative = declaration
            .qualified_name
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if relative.len() != usize::from(associated) + 1
            || relative.last() != Some(&declaration.name)
            || !relative
                .iter()
                .all(|name| rift_core::is_portable_name(name))
        {
            continue;
        }
        let candidate = Candidate {
            name: declaration.name.clone(),
            relative,
            kind: ItemKind {
                exact: "constant",
                kinds: &[SymbolKind::Value],
                child: associated,
            },
            visibility: declaration.visibility.clone(),
            variant_owner: None,
            member_owner: None,
        };
        output
            .entry((associated, declaration.name.as_str()))
            .and_modify(|entry| *entry = None)
            .or_insert(Some(candidate));
    }
    output
}

fn placement(
    elements: &[SyntaxElement],
    element: &SyntaxElement,
    associated: &BTreeSet<ByteRange>,
) -> Option<bool> {
    if element.kind != "const_item" || element.has_error {
        return None;
    }
    let parent = elements.get(element.parent?)?;
    if parent.kind == "source_file" {
        return Some(false);
    }
    if parent.kind != "declaration_list" || parent.has_error {
        return None;
    }
    let owner = elements.get(parent.parent?)?;
    (owner.parent == Some(0) && associated.contains(&owner.range)).then_some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_require_a_complete_direct_emitted_scope() {
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let limits = SyntaxLimits::DEFAULT;
        for (expansion, count) in [
            ("const TOP: u8 = 1; impl Board { const SLOT: u8 = 2; }", 2),
            (
                "const TOP: u8 = 1; impl Trait for Board { const SLOT: u8 = 2; }",
                1,
            ),
            (
                "const TOP: u8 = 1; impl Board<u8> { const SLOT: u8 = 2; }",
                1,
            ),
            ("mod hidden { const TOP: u8 = 1; }", 0),
            ("const TOP: u8 = 1; const TOP: u8 = 2;", 0),
            (
                "impl Board { const TOP: u8 = 1; } impl Other { const TOP: u8 = 2; }",
                0,
            ),
            ("const TOP: u8 = 1; impl Board { const SLOT: u8 = ; }", 0),
        ] {
            let observed = parse(&path, expansion, limits).map_or(0, |document| {
                emitted_constants(&document, expansion, limits)
                    .into_values()
                    .flatten()
                    .count()
            });
            assert_eq!(observed, count, "{expansion}");
        }
    }
}
