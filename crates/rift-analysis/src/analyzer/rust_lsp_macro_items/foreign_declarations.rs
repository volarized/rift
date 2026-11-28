//! Checks authored signatures against complete selected Rust foreign declarations.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{FileDigest, ProjectPath, SourceRange};
use rift_protocol::{
    identity::SymbolIdentity,
    index::{PackagePublication, PackageSourceUnit, PackageSymbol},
    read::{SourceKind, SymbolId, SymbolKind},
};
use rift_syntax::{ByteRange, SyntaxElement, SyntaxLimits};

use super::{
    Candidate, Definition, ItemKind, RustLspMacroSite, parse, range, same_source, text_at,
    variant_fields::TokenGroups,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Function,
    Parameter,
}

struct Signature<'source> {
    words: Vec<&'source str>,
    function: usize,
    name: usize,
    parameters: Vec<usize>,
    visibility: Option<String>,
}

/// Each original signature has exactly the same tokens as a parsed emitted
/// signature. Delimiters and separators belonging to the invocation are excluded.
pub(super) fn candidates(
    path: &ProjectPath,
    original: &str,
    invocation: SourceRange,
    expansion: &str,
    limits: SyntaxLimits,
) -> BTreeMap<SourceRange, (Role, Candidate)> {
    let Some(mut signatures) = emitted_signatures(path, expansion, limits) else {
        return BTreeMap::new();
    };
    if signatures.is_empty() {
        return BTreeMap::new();
    }
    let Some(TokenGroups(tokens)) = TokenGroups::capture(original, invocation, limits) else {
        return BTreeMap::new();
    };
    let mut written = BTreeMap::new();
    for (index, keyword) in tokens.iter().enumerate() {
        if keyword.text != "fn" {
            continue;
        }
        let Some(group) = keyword
            .parent
            .filter(|&parent| tokens[parent].parent.is_none())
        else {
            continue;
        };
        let Some(name) = tokens.get(index + 1) else {
            continue;
        };
        if !signatures.contains_key(name.text) {
            continue;
        }
        written
            .entry(name.text)
            .and_modify(|value| *value = None)
            .or_insert(Some((index, group)));
    }

    let mut output = BTreeMap::new();
    for (name, location) in written {
        let Some((index, group)) = location else {
            continue;
        };
        let Some(signature) = signatures.remove(name).flatten() else {
            continue;
        };
        let Some(start) = index.checked_sub(signature.function) else {
            continue;
        };
        let end = start + signature.words.len();
        let Some(authored) = tokens.get(start..end) else {
            continue;
        };
        if start <= group
            || (start != group + 1
                && !tokens.get(start - 1).is_some_and(|token| {
                    token.parent == Some(group) && matches!(token.text, "," | ";" | ":")
                }))
            || !tokens.get(end).is_some_and(|token| {
                token.parent == Some(group) && matches!(token.text, "," | ";" | ")" | "]" | "}")
            })
            || !authored.iter().map(|token| token.text).eq(signature.words)
        {
            continue;
        }
        let name_range = authored[signature.name].range;
        output.insert(
            name_range,
            (
                Role::Function,
                Candidate {
                    name: name.to_owned(),
                    relative: vec![name.to_owned()],
                    kind: ItemKind {
                        exact: "function",
                        kinds: &[SymbolKind::Value, SymbolKind::Callable],
                        child: false,
                    },
                    visibility: signature.visibility,
                    variant_owner: None,
                    member_owner: None,
                },
            ),
        );
        for index in signature.parameters {
            let parameter = &authored[index];
            output.insert(
                parameter.range,
                (
                    Role::Parameter,
                    Candidate {
                        name: parameter.text.to_owned(),
                        relative: vec![name.to_owned(), format!("{}#parameter0", parameter.text)],
                        kind: ItemKind {
                            exact: "parameter_binding",
                            kinds: &[SymbolKind::Value, SymbolKind::Parameter],
                            child: true,
                        },
                        visibility: None,
                        variant_owner: None,
                        member_owner: Some(name_range),
                    },
                ),
            );
        }
    }
    output
}

fn emitted_signatures<'a>(
    path: &ProjectPath,
    expansion: &'a str,
    limits: SyntaxLimits,
) -> Option<BTreeMap<&'a str, Option<Signature<'a>>>> {
    let document = parse(path, expansion, limits)?;
    let elements = document.syntax_elements();
    let mut children = vec![Vec::new(); elements.len()];
    for (index, element) in elements.iter().enumerate() {
        if let Some(parent) = element.parent {
            children[parent].push(index);
        }
    }
    let mut signatures = BTreeMap::new();
    for (index, item) in elements.iter().enumerate() {
        if item.kind != "function_signature_item" || !foreign_scope(elements, item) {
            continue;
        }
        let Some(signature) = signature(expansion, index, elements, &children, limits) else {
            continue;
        };
        signatures
            .entry(signature.words[signature.name])
            .and_modify(|value| *value = None)
            .or_insert(Some(signature));
    }
    Some(signatures)
}

fn foreign_scope(elements: &[SyntaxElement], item: &SyntaxElement) -> bool {
    let Some(list) = item.parent.and_then(|index| elements.get(index)) else {
        return false;
    };
    list.kind == "declaration_list"
        && list
            .parent
            .and_then(|index| elements.get(index))
            .is_some_and(|block| {
                block.kind == "foreign_mod_item" && !block.has_error && block.parent == Some(0)
            })
}

fn signature<'source>(
    source: &'source str,
    index: usize,
    elements: &[SyntaxElement],
    children: &[Vec<usize>],
    limits: SyntaxLimits,
) -> Option<Signature<'source>> {
    let item = &elements[index];
    let name = children[index]
        .iter()
        .map(|&i| &elements[i])
        .find(|el| el.kind == "identifier")?;
    let list = *children[index]
        .iter()
        .find(|&&i| elements[i].kind == "parameters")?;
    let visibility = children[index]
        .iter()
        .map(|&i| &elements[i])
        .find(|el| el.kind == "visibility_modifier")
        .and_then(|el| text_at(source, range(el.range)?))
        .map(str::to_owned);
    let mut words = Vec::new();
    if !rift_syntax::tokens(source, item.range, limits, |token| {
        let Some(text) = range(token).and_then(|span| text_at(source, span)) else {
            return false;
        };
        words.push((token, text));
        true
    }) || words.pop()?.1 != ";"
    {
        return None;
    }
    let name_index = words
        .binary_search_by_key(&name.range, |&(span, _)| span)
        .ok()?;
    let function = name_index.checked_sub(1)?;
    if words[function].1 != "fn" || !rift_core::is_portable_name(words[name_index].1) {
        return None;
    }
    let mut parameters = Vec::new();
    let mut names = BTreeSet::new();
    for &position in &children[list] {
        if elements[position].kind != "parameter" {
            continue;
        }
        let first = elements.get(*children[position].first()?)?;
        if first.kind != "identifier" {
            return None;
        }
        let index = words
            .binary_search_by_key(&first.range, |&(span, _)| span)
            .ok()?;
        if !rift_core::is_portable_name(words[index].1) || !names.insert(words[index].1) {
            return None;
        }
        parameters.push(index);
    }
    Some(Signature {
        words: words.into_iter().map(|(_, text)| text).collect(),
        function,
        name: name_index,
        parameters,
        visibility,
    })
}

pub(super) fn confirm(
    candidates: BTreeMap<SourceRange, (Role, Candidate)>,
    labels: &BTreeMap<SourceRange, Option<Role>>,
    confirmed: &mut Vec<SourceRange>,
) -> BTreeMap<SourceRange, Candidate> {
    let incomplete = candidates
        .iter()
        .filter_map(|(&range, (role, candidate))| {
            (labels.get(&range) != Some(&Some(*role)))
                .then_some(candidate.member_owner.unwrap_or(range))
        })
        .collect::<BTreeSet<_>>();
    candidates
        .into_iter()
        .filter_map(|(range, (_, candidate))| {
            if incomplete.contains(&candidate.member_owner.unwrap_or(range)) {
                return None;
            }
            confirmed.push(range);
            Some((range, candidate))
        })
        .collect()
}

pub(super) fn labels(
    observed: &[(SourceRange, bool)],
    limits: SyntaxLimits,
) -> BTreeMap<SourceRange, Option<Role>> {
    if observed.len() > limits.syntax_elements_max() {
        return BTreeMap::new();
    }
    let mut output = BTreeMap::new();
    for &(span, function) in observed {
        let role = if function {
            Role::Function
        } else {
            Role::Parameter
        };
        output
            .entry(span)
            .and_modify(|prior| *prior = None)
            .or_insert(Some(role));
    }
    output
}

struct Outline<'a> {
    root: &'a PackageSourceUnit,
    start: u64,
    end: u64,
    imported: bool,
}

#[derive(Default)]
pub(super) struct ImportedOwners<'a> {
    outlines: BTreeMap<&'a SymbolId, Option<Outline<'a>>>,
    files: BTreeSet<(&'a SymbolId, &'a str)>,
}

impl<'a> ImportedOwners<'a> {
    pub(super) fn capture(
        publication: &'a PackagePublication,
        declarations: &BTreeMap<&str, Vec<&'a PackageSymbol>>,
        limits: SyntaxLimits,
    ) -> Self {
        let mut proof = Self::default();
        for unit in &publication.units {
            if !unit.source_complete || unit.language.name != "rust" {
                continue;
            }
            let Some(rows) = declarations.get(unit.unit.as_str()) else {
                continue;
            };
            let Ok(path) = ProjectPath::new(&unit.path.0) else {
                continue;
            };
            if !same_source(
                publication,
                &path,
                FileDigest::of(unit.source.as_bytes()),
                unit,
            ) {
                continue;
            }
            for row in rows {
                if row.syntax_element_kind.as_deref() == Some("source_file")
                    && row.origin.source_kind == SourceKind::Authored
                {
                    proof.files.insert((&row.symbol, unit.unit.as_str()));
                }
            }
            if !unit.source.contains("macro_use") {
                continue;
            }
            let Some(original) = parse(&path, &unit.source, limits) else {
                continue;
            };
            let modules = original
                .facts()
                .symbols()
                .iter()
                .filter(|symbol| {
                    symbol.syntax_element_kind == Some("mod_item")
                        && symbol.body_range.is_none()
                        && symbol.container.is_none()
                })
                .map(|symbol| (symbol.item_range, symbol))
                .collect::<BTreeMap<_, _>>();
            for row in rows {
                if row.syntax_element_kind.as_deref() != Some("mod_item")
                    || row.origin.source_kind != SourceKind::Authored
                {
                    continue;
                }
                let Some(module) = modules.get(&ByteRange {
                    start: row.range.start,
                    end: row.range.end,
                }) else {
                    continue;
                };
                let prefix = ByteRange {
                    start: module.range.start,
                    end: module.item_range.start,
                };
                let imported = macro_use(&unit.source, prefix, limits);
                let outline = Outline {
                    root: unit,
                    start: row.range.start,
                    end: row.range.end,
                    imported,
                };
                proof
                    .outlines
                    .entry(&row.symbol)
                    .and_modify(|old| *old = None)
                    .or_insert(Some(outline));
            }
        }
        proof
    }

    pub(super) fn allows(
        &self,
        definition: &Definition<'_>,
        site: &RustLspMacroSite,
        invoking_owner: &PackageSymbol,
    ) -> bool {
        if site.name.start() != site.invocation.start()
            || invoking_owner.syntax_element_kind.as_deref() != Some("source_file")
        {
            return false;
        }
        let mut path = definition.identity.qualified_path().to_vec();
        if path.pop().is_none() || path.len() < 2 {
            return false;
        }
        let Ok(defining_module) = SymbolIdentity::new(
            definition.identity.owner().clone(),
            definition.identity.language().clone(),
            path,
        ) else {
            return false;
        };
        let defining_module = SymbolId::from_identity(&defining_module);
        if !self
            .files
            .contains(&(&defining_module, definition.unit.unit.as_str()))
        {
            return false;
        }
        self.outlines
            .get(&defining_module)
            .and_then(Option::as_ref)
            .zip(
                self.outlines
                    .get(&invoking_owner.symbol)
                    .and_then(Option::as_ref),
            )
            .is_some_and(|(defined, invoking)| {
                defined.imported
                    && defined.root.unit == invoking.root.unit
                    && defined.end <= invoking.start
            })
    }
}

fn macro_use(source: &str, prefix: ByteRange, limits: SyntaxLimits) -> bool {
    let mut expected = ["#", "[", "macro_use", "]"].into_iter();
    rift_syntax::tokens(source, prefix, limits, |token| {
        range(token).and_then(|span| text_at(source, span)) == expected.next()
    }) && expected.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_signatures_require_exact_tokens_and_scope() {
        let source =
            "make! { 3, pub fn pick(item: u32, count: u16) -> i32, pub fn put(item: u8) -> i32, }";
        let path = ProjectPath::new("src/lib.rs").expect("source path");
        let outer = SourceRange::new(0, source.len() as u64).expect("full invocation");
        for (expansion, expected) in [
            (
                "unsafe extern \"C\" { pub fn pick(item:u32,count:u16)->i32; pub fn put(item:u8)->i32; }",
                vec!["pick", "item", "count", "put", "item"],
            ),
            (
                "unsafe extern \"system\" { pub fn pick(item:u32,count:u8)->i32; pub fn put(item:u8)->i32; }",
                vec!["put", "item"],
            ),
            (
                "mod other { unsafe extern \"C\" { pub fn put(item:u8)->i32; } }",
                vec![],
            ),
            (
                "unsafe extern \"C\" { pub fn put(item:u8)->i32; pub fn put(item:u8)->i32; }",
                vec![],
            ),
            (
                "unsafe extern \"C\" { pub fn pick(item:); pub fn put(item:u8)->i32;",
                vec![],
            ),
            (
                "unsafe extern \"C\" { pub fn generated_only(item:u8)->i32; }",
                vec![],
            ),
        ] {
            let matching = candidates(&path, source, outer, expansion, SyntaxLimits::DEFAULT);
            let visible = matching
                .keys()
                .filter_map(|&span| text_at(source, span))
                .collect::<Vec<_>>();
            assert_eq!(visible, expected, "{expansion}");
            let parents = matching
                .values()
                .filter_map(|(_, candidate)| candidate.member_owner)
                .collect::<BTreeSet<_>>();
            if visible.len() == 5 {
                assert_eq!(parents.len(), 2);
            }
        }
    }
}
