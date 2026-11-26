//! Places written method names selected by a single local macro rule.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_core::SymbolId;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ByteRange, RustSyntaxProvider, SyntaxElement, SyntaxLimits, SyntaxProvider, SyntaxSource,
    SyntaxSymbol,
};

use super::super::{PreparedNamespace, SelectedFile};
use super::configuration::Configurations;
use super::macros::{consume, group, text_range};
use super::{Modules, source_identifier};

pub(super) fn retain(
    input: &crate::NamespaceInput<'_>,
    files: &[SelectedFile<'_>],
    configurations: &Configurations<'_>,
    admitted: &BTreeMap<String, Option<bool>>,
    prepared: &mut PreparedNamespace,
) {
    if !matches!(input.owner(), SymbolOwner::Package { .. }) {
        return;
    }
    let modules = Modules {
        configurations,
        admitted,
    };
    let mut nominal = BTreeSet::new();
    for file in files {
        let Some(anchors) = prepared.anchors.get(file.path.as_str()) else {
            continue;
        };
        for symbol in file.syntax.symbols() {
            if matches!(symbol.kind, "struct" | "enum" | "union_item")
                && let Some(identity) = anchors.get(&symbol.qualified_name)
            {
                nominal.insert(identity.clone());
            }
        }
    }
    for file in files {
        let Some(anchors) = prepared.anchors.get_mut(file.path.as_str()) else {
            continue;
        };
        if !file.source.contains("macro_rules")
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
            || !file
                .syntax
                .symbols()
                .iter()
                .any(|symbol| symbol.kind == "impl_item")
            || modules.file(file) != Some(true)
        {
            continue;
        }
        let methods = collect(file, anchors, &nominal, &modules, input.syntax());
        if !methods.is_empty() {
            for (method, identity) in &methods {
                anchors.insert(method.qualified_name.clone(), identity.clone());
            }
            prepared.rust_macro_methods.insert(
                file.path.as_str().to_owned(),
                methods.into_iter().map(|(symbol, _)| symbol).collect(),
            );
        }
    }
}

#[derive(Clone, Copy)]
enum Pattern<'source> {
    Literal,
    Header { attributes: Option<&'source str> },
}

#[derive(Clone, Copy)]
struct Rule<'source> {
    pattern: Pattern<'source>,
    public: bool,
    at_least_one: bool,
}

struct Tokens<'source> {
    source: &'source str,
    limits: SyntaxLimits,
    left: usize,
}

impl<'source> Tokens<'source> {
    fn read(&mut self, range: ByteRange) -> Option<Vec<(ByteRange, &'source str)>> {
        let mut rows = Vec::new();
        rift_syntax::tokens(self.source, range, self.limits, |range| {
            let Some(value) = text_range(self.source, range) else {
                return false;
            };
            let Some(left) = self.left.checked_sub(1) else {
                return false;
            };
            self.left = left;
            rows.push((range, value));
            true
        })
        .then_some(rows)
    }

    fn values(&mut self, range: ByteRange) -> Option<Vec<&'source str>> {
        Some(
            self.read(range)?
                .into_iter()
                .map(|(_, value)| value)
                .collect(),
        )
    }
}

fn collect(
    file: &SelectedFile<'_>,
    anchors: &BTreeMap<String, SymbolId>,
    nominal: &BTreeSet<SymbolId>,
    modules: &Modules<'_, '_>,
    limits: SyntaxLimits,
) -> Vec<(SyntaxSymbol, SymbolId)> {
    let Ok(document) = RustSyntaxProvider::default().analyze(
        SyntaxSource {
            path: file.path,
            text: file.source,
        },
        limits,
    ) else {
        return Vec::new();
    };
    let rows = document.syntax_elements();
    if rows
        .first()
        .is_none_or(|root| root.kind != "source_file" || root.has_error)
    {
        return Vec::new();
    }
    let mut children = vec![Vec::new(); rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if let Some(parent) = row.parent {
            children[parent].push(index);
        }
    }
    let mut tokens = Tokens {
        source: file.source,
        limits,
        left: limits.syntax_elements_max(),
    };
    let implementations = file
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == "impl_item")
        .map(|symbol| (symbol.item_range, symbol))
        .collect::<BTreeMap<_, _>>();
    let mut definitions = BTreeMap::new();
    let mut methods = BTreeMap::new();
    let mut attributes = Vec::new();
    for &index in &children[0] {
        let row = &rows[index];
        if row.kind.ends_with("_comment") {
            continue;
        }
        if row.kind == "attribute_item" {
            attributes.push(row.range);
            continue;
        }
        let plain = attributes
            .iter()
            .all(|range| inert_attribute(file.source, *range));
        attributes.clear();
        match row.kind {
            "macro_definition" => {
                let Some(name) = children[index]
                    .iter()
                    .map(|&child| &rows[child])
                    .find(|child| child.kind == "identifier")
                    .and_then(|child| text_range(file.source, child.range))
                else {
                    definitions.clear();
                    continue;
                };
                let checked = (plain && modules.declared(file, row.range) == Some(true))
                    .then(|| rule(&mut tokens, row.range, name))
                    .flatten();
                definitions.insert(name, checked);
            }
            "macro_invocation" | "expression_statement" => definitions.clear(),
            "impl_item" if plain && !definitions.is_empty() => {
                let Some(implementation) = implementations.get(&row.range).copied() else {
                    continue;
                };
                let Some(owner) = nominal_owner(implementation, anchors, nominal)
                    .filter(|_| modules.declared(file, row.range) == Some(true))
                else {
                    continue;
                };
                retain_impl(
                    implementation,
                    &owner,
                    (rows, &children),
                    index,
                    &definitions,
                    &mut tokens,
                    |method, identity| {
                        if !anchors.contains_key(&method.qualified_name) {
                            methods
                                .entry(identity)
                                .and_modify(|previous| *previous = None)
                                .or_insert(Some(method));
                        }
                    },
                );
            }
            _ => {}
        }
    }
    methods
        .into_iter()
        .filter_map(|(identity, method)| Some((method?, identity)))
        .collect()
}

fn nominal_owner(
    implementation: &SyntaxSymbol,
    anchors: &BTreeMap<String, SymbolId>,
    nominal: &BTreeSet<SymbolId>,
) -> Option<SymbolIdentity> {
    let owner = anchors.get(&implementation.qualified_name)?;
    nominal
        .contains(owner)
        .then(|| SymbolIdentity::parse(owner.as_str()).ok())
        .flatten()
}

fn inert_attribute(source: &str, range: ByteRange) -> bool {
    let Some(value) = text_range(source, range).and_then(super::macros::tokens) else {
        return false;
    };
    matches!(
        value.as_slice(),
        [
            "#",
            "[",
            "doc"
                | "allow"
                | "expect"
                | "warn"
                | "deny"
                | "forbid"
                | "inline"
                | "must_use"
                | "deprecated",
            ..,
            "]"
        ]
    )
}

fn rule<'source>(
    tokens: &mut Tokens<'source>,
    range: ByteRange,
    name: &str,
) -> Option<Rule<'source>> {
    let values = tokens.values(range)?;
    let mut outer = values.as_slice();
    if !consume(&mut outer, &["macro_rules", "!", name]) {
        return None;
    }
    let mut arm = group(&mut outer, "{", "}")?;
    if !outer.is_empty() && outer != [";"] {
        return None;
    }
    let mut pattern = group(&mut arm, "(", ")")?;
    if !consume(&mut pattern, &["$"]) {
        return None;
    }
    let mut pattern_body = group(&mut pattern, "(", ")")?;
    let [repeat @ ("+" | "*")] = pattern else {
        return None;
    };
    let mut attributes = None;
    if pattern_body.starts_with(&["$", "(", "#"]) && consume(&mut pattern_body, &["$"]) {
        let ["#", "[", "$", name, ":", "meta", "]"] = group(&mut pattern_body, "(", ")")? else {
            return None;
        };
        if !consume(&mut pattern_body, &["*"]) {
            return None;
        }
        attributes = Some(*name);
    }
    let (pattern, written, fields) = match pattern_body {
        [
            "$",
            written,
            ":",
            "ident",
            "=",
            ">",
            "$",
            value,
            ":",
            "literal",
            ";",
        ] if attributes.is_none() && written != value => (Pattern::Literal, *written, None),
        [
            "fn",
            "$",
            written,
            ":",
            "ident",
            "(",
            "$",
            field,
            ":",
            "ident",
            ":",
            "$",
            ty,
            ":",
            "ty",
            ")",
            ";",
        ] if written != field && written != ty && field != ty => (
            Pattern::Header { attributes },
            *written,
            Some((*field, *ty)),
        ),
        _ => return None,
    };
    if !consume(&mut arm, &["=", ">"]) {
        return None;
    }
    let mut replacement = group(&mut arm, "{", "}")?;
    if !arm.is_empty() && arm != [";"] || !consume(&mut replacement, &["$"]) {
        return None;
    }
    let output = group(&mut replacement, "(", ")")?;
    if replacement != [*repeat] {
        return None;
    }
    let public = method_output(output, attributes, written, fields)?;
    Some(Rule {
        pattern,
        public,
        at_least_one: *repeat == "+",
    })
}

fn method_output(
    mut output: &[&str],
    attributes: Option<&str>,
    written: &str,
    fields: Option<(&str, &str)>,
) -> Option<bool> {
    if let Some(name) = attributes
        && !consume(&mut output, &["$", "(", "#", "[", "$", name, "]", ")", "*"])
    {
        return None;
    }
    while consume(&mut output, &["#"]) {
        let attribute = group(&mut output, "[", "]")?;
        if !matches!(
            attribute,
            [
                "doc" | "allow" | "expect" | "inline" | "must_use" | "deprecated",
                ..
            ]
        ) || attribute.contains(&"$")
        {
            return None;
        }
    }
    let public = consume(&mut output, &["pub"]);
    let _ = consume(&mut output, &["const"]);
    if !consume(&mut output, &["fn", "$", written]) {
        return None;
    }
    let receiver = group(&mut output, "(", ")")?;
    let valid_receiver = if let Some((field, ty)) = fields {
        receiver == ["self", ",", "$", field, ":", "$", ty]
    } else {
        matches!(receiver, ["self"] | ["&", "self"] | ["&", "mut", "self"])
    };
    if !valid_receiver || !consume(&mut output, &["-", ">"]) {
        return None;
    }
    let [result, rest @ ..] = output else {
        return None;
    };
    source_identifier(result)?;
    let mut rest = rest;
    group(&mut rest, "{", "}")?;
    rest.is_empty().then_some(public)
}

fn retain_impl<'source>(
    implementation: &SyntaxSymbol,
    owner: &SymbolIdentity,
    tree: (&[SyntaxElement], &[Vec<usize>]),
    index: usize,
    definitions: &BTreeMap<&'source str, Option<Rule<'source>>>,
    tokens: &mut Tokens<'source>,
    mut publish: impl FnMut(SyntaxSymbol, SymbolId),
) {
    let (rows, children) = tree;
    let Some(&body_index) = children[index]
        .iter()
        .find(|&&child| rows[child].kind == "declaration_list")
    else {
        return;
    };
    let body = &rows[body_index];
    let header = ByteRange {
        start: rows[index].range.start,
        end: body.range.start,
    };
    if !matches!(tokens.values(header).as_deref(), Some(["impl", name]) if source_identifier(name).is_some())
    {
        return;
    }
    let mut attributes = Vec::new();
    for &index in &children[body_index] {
        let row = &rows[index];
        if row.kind.ends_with("_comment") {
            continue;
        }
        if row.kind == "attribute_item" {
            attributes.push(row.range);
            continue;
        }
        let plain = attributes
            .iter()
            .all(|range| inert_attribute(tokens.source, *range));
        attributes.clear();
        let invocation = if row.kind == "macro_invocation" {
            Some(row)
        } else if row.kind == "expression_statement" {
            children[index]
                .iter()
                .find_map(|&child| (rows[child].kind == "macro_invocation").then_some(&rows[child]))
        } else {
            continue;
        };
        let Some(invocation) = invocation else {
            return;
        };
        let Some(values) = tokens.read(invocation.range) else {
            return;
        };
        let Some((_, name)) = values.first() else {
            return;
        };
        let Some(Some(rule)) = definitions.get(name) else {
            return;
        };
        let Some(heads) = plain.then(|| invocation_heads(&values, *rule)).flatten() else {
            return;
        };
        for (range, written) in heads {
            let Some(name) = source_identifier(written) else {
                continue;
            };
            let mut path = owner.qualified_path().to_vec();
            path.push(name.clone());
            let Ok(identity) =
                SymbolIdentity::new(owner.owner().clone(), owner.language().clone(), path)
                    .and_then(|identity| SymbolId::new(identity.wire_identity()))
            else {
                continue;
            };
            let mut kinds = vec![SymbolKind::Value, SymbolKind::Callable];
            if rule.public {
                kinds.push(SymbolKind::Public);
            }
            publish(
                SyntaxSymbol {
                    qualified_name: format!("{}::{name}", implementation.qualified_name),
                    container: Some(implementation.qualified_name.clone()),
                    name,
                    declaration_ordinal: 0,
                    kind: "function",
                    syntax_element_kind: Some("identifier"),
                    kinds,
                    visibility: rule.public.then(|| "pub".to_owned()),
                    range,
                    item_range: range,
                    name_range: Some(range),
                    body_range: None,
                    signatures: Arc::from([]),
                    documentation: Arc::from([]),
                    documentation_ranges: Vec::new(),
                    module_path: None,
                    python_overload: None,
                },
                identity,
            );
        }
    }
}

fn invocation_heads<'source>(
    rows: &[(ByteRange, &'source str)],
    rule: Rule<'_>,
) -> Option<Vec<(ByteRange, &'source str)>> {
    let values = rows.iter().map(|(_, value)| *value).collect::<Vec<_>>();
    let [_, "!", open @ ("{" | "(" | "["), rest @ ..] = values.as_slice() else {
        return None;
    };
    let close = match *open {
        "{" => "}",
        "(" => ")",
        _ => "]",
    };
    let (tail, body) = rest.split_last()?;
    if *tail != close || rule.at_least_one && body.is_empty() {
        return None;
    }
    let mut result = Vec::new();
    let mut body = body;
    while !body.is_empty() {
        if matches!(
            rule.pattern,
            Pattern::Header {
                attributes: Some(_)
            }
        ) {
            while consume(&mut body, &["#"]) {
                let attribute = group(&mut body, "[", "]")?;
                if !matches!(
                    attribute,
                    [
                        "doc" | "allow" | "expect" | "inline" | "must_use" | "deprecated",
                        ..
                    ]
                ) {
                    return None;
                }
            }
        }
        if matches!(rule.pattern, Pattern::Header { .. }) && !consume(&mut body, &["fn"]) {
            return None;
        }
        let offset = values.len().checked_sub(body.len() + 1)?;
        let (name, rest) = body.split_first()?;
        source_identifier(name)?;
        body = rest;
        match rule.pattern {
            Pattern::Literal => {
                if !consume(&mut body, &["=", ">"]) {
                    return None;
                }
                let (literal, rest) = body.split_first()?;
                if !literal.as_bytes().iter().all(u8::is_ascii_digit) {
                    return None;
                }
                body = rest;
            }
            Pattern::Header { .. } => {
                let [field, ":", ty] = group(&mut body, "(", ")")? else {
                    return None;
                };
                source_identifier(field)?;
                source_identifier(ty)?;
            }
        }
        if !consume(&mut body, &[";"]) {
            return None;
        }
        result.push(rows[offset]);
    }
    Some(result)
}
