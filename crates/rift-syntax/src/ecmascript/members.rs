//! Callable property assignments whose receiver has an unambiguous lexical binding.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_error::RiftError;
use rift_protocol::read::Language;
use tree_sitter::Node;

use super::{EcmaScriptGrammarField, EcmaScriptRules, EcmaScriptSymbolKind};
use crate::extract::{self, Declaration, GrammarRules};
use crate::{ByteRange, StaticBinding, StaticBindingKind, StaticFacts, SyntaxSymbol};

struct Bindings<'source> {
    scopes: BTreeMap<(ByteRange, &'source str), Option<&'source StaticBinding>>,
    symbols: BTreeMap<ByteRange, Option<usize>>,
    replaced: BTreeSet<&'source str>,
    blocked: BTreeSet<ByteRange>,
}

impl<'source> Bindings<'source> {
    fn new(source: &'source str, facts: &'source StaticFacts, symbols: &[SyntaxSymbol]) -> Self {
        let mut scopes = BTreeMap::new();
        for binding in facts.bindings() {
            let Some(name) = text(source, binding.name) else {
                continue;
            };
            scopes
                .entry((binding.scope, name))
                .and_modify(|entry| *entry = None)
                .or_insert(Some(binding));
        }
        let mut names = BTreeMap::new();
        for (index, symbol) in symbols.iter().enumerate() {
            if let Some(name) = symbol.name_range {
                names
                    .entry(name)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(index));
            }
        }
        let members = facts
            .members()
            .iter()
            .map(|member| (member.range, member))
            .collect::<BTreeMap<_, _>>();
        let mut replaced = BTreeSet::new();
        for &range in facts.assignments() {
            if let Some(member) = members.get(&range) {
                let property = member.member.and_then(|range| text(source, range));
                if (property.is_none() || property == Some("prototype"))
                    && let Some(name) = text(source, member.receiver)
                {
                    replaced.insert(name);
                }
            } else if let Some(name) = text(source, range) {
                replaced.insert(name);
            }
        }
        Self {
            scopes,
            symbols: names,
            replaced,
            blocked: facts.unsupported_bindings().iter().copied().collect(),
        }
    }

    fn owner(&self, receiver: Node<'_>, source: &str) -> Option<usize> {
        let name = source.get(receiver.byte_range())?;
        if self.replaced.contains(name) {
            return None;
        }
        let mut ancestor = receiver.parent();
        // The shared walk has admitted the source depth before this lookup.
        while let Some(scope) = ancestor {
            if self.blocked.contains(&extract::byte_range(scope).ok()?) {
                return None;
            }
            if let Some(binding) = self.scopes.get(&(extract::byte_range(scope).ok()?, name)) {
                let binding = (*binding)?;
                if binding.kind != StaticBindingKind::Local {
                    return None;
                }
                return self.symbols.get(&binding.name).copied().flatten();
            }
            ancestor = scope.parent();
        }
        None
    }
}

#[derive(Clone, Copy)]
struct MemberFunction<'tree> {
    receiver: Node<'tree>,
    name: Node<'tree>,
    value: Node<'tree>,
}

fn prototype_function<'tree>(
    rules: &EcmaScriptRules<'_>,
    assignment: Node<'tree>,
    source: &str,
) -> Option<MemberFunction<'tree>> {
    let kinds = rules.kinds;
    let left = kinds.child(assignment, EcmaScriptGrammarField::Left)?;
    if left.kind_id() != kinds.member_expression {
        return None;
    }
    let name = kinds.child(left, EcmaScriptGrammarField::Property)?;
    if name.kind_id() != kinds.property_identifier {
        return None;
    }
    let prototype = kinds.child(left, EcmaScriptGrammarField::Object)?;
    if prototype.kind_id() != kinds.member_expression {
        return None;
    }
    let property = kinds.child(prototype, EcmaScriptGrammarField::Property)?;
    if source.get(property.byte_range()) != Some("prototype") {
        return None;
    }
    let receiver = kinds.child(prototype, EcmaScriptGrammarField::Object)?;
    if receiver.kind_id() != kinds.identifier {
        return None;
    }
    let value = kinds.expression_value(kinds.child(assignment, EcmaScriptGrammarField::Right)?);
    kinds
        .function_values
        .contains(&value.kind_id())
        .then_some(MemberFunction {
            receiver,
            name,
            value,
        })
}

pub(super) fn record(
    rules: &EcmaScriptRules<'_>,
    source: &str,
    language: &Language,
    facts: &StaticFacts,
    assignments: &[Node<'_>],
    symbols: &mut Vec<SyntaxSymbol>,
) -> Result<(), RiftError> {
    let functions = assignments
        .iter()
        .filter_map(|&assignment| {
            Some((assignment, prototype_function(rules, assignment, source)?))
        })
        .collect::<Vec<_>>();
    if functions.is_empty() {
        return Ok(());
    }
    let bindings = Bindings::new(source, facts, symbols);
    let mut ordered = (0..symbols.len()).collect::<Vec<_>>();
    ordered.sort_by_key(|&index| symbols[index].range.start);
    let mut containers = BTreeMap::new();
    let mut names = BTreeMap::new();
    for symbol in symbols.iter() {
        *names
            .entry(symbol.qualified_name.clone())
            .or_insert(0_usize) += 1;
    }
    // Assignments and declarations share the admitted syntax count. Renaming touches only
    // declarations in a callable's source interval, at most once per admitted ancestor depth.
    for (assignment, function) in functions {
        let Some(owner) = bindings.owner(function.receiver, source) else {
            continue;
        };
        if !matches!(symbols[owner].kind, "function" | "class")
            || names.get(&symbols[owner].qualified_name) != Some(&1)
        {
            continue;
        }
        let container = symbols[owner].qualified_name.clone();
        let Some(name) = source.get(function.name.byte_range()) else {
            continue;
        };
        let qualified = extract::qualify(".", &container, name);
        let parent = qualification(rules, assignment, source, &containers);
        rename_children(
            symbols,
            &ordered,
            extract::byte_range(function.value)?,
            &parent,
            &qualified,
            &mut names,
        );
        containers.insert(assignment.id(), qualified.clone());
        *names.entry(qualified.clone()).or_default() += 1;
        symbols.push(method(
            assignment, function, name, container, qualified, source, language,
        )?);
    }
    Ok(())
}

fn qualification(
    rules: &EcmaScriptRules<'_>,
    assignment: Node<'_>,
    source: &str,
    containers: &BTreeMap<usize, String>,
) -> String {
    let mut parts = Vec::new();
    let mut ancestor = assignment.parent();
    // Parent traversal is bounded by the shared walk's admitted depth.
    while let Some(element) = ancestor {
        if let Some(container) = containers.get(&element.id()) {
            parts.push(container.clone());
            break;
        }
        if let Some(name) = rules.container_name(element, source) {
            parts.push(name);
        }
        ancestor = element.parent();
    }
    parts.reverse();
    parts.join(".")
}

fn rename_children(
    symbols: &mut [SyntaxSymbol],
    ordered: &[usize],
    range: ByteRange,
    parent: &str,
    qualified: &str,
    names: &mut BTreeMap<String, usize>,
) {
    let start = ordered.partition_point(|&index| symbols[index].range.start < range.start);
    let end = ordered.partition_point(|&index| symbols[index].range.start < range.end);
    for &index in &ordered[start..end] {
        let symbol = &mut symbols[index];
        if !range.encloses(symbol.item_range) {
            continue;
        }
        let updated = replace_parent(&symbol.qualified_name, parent, qualified);
        let previous = std::mem::replace(&mut symbol.qualified_name, updated);
        let count = names
            .get_mut(&previous)
            .expect("every declaration has a name count");
        *count = count
            .checked_sub(1)
            .expect("a retained declaration has a positive name count");
        if *count == 0 {
            names.remove(&previous);
        }
        *names.entry(symbol.qualified_name.clone()).or_default() += 1;
        symbol.container = Some(symbol.container.as_deref().map_or_else(
            || qualified.to_owned(),
            |name| replace_parent(name, parent, qualified),
        ));
    }
}

fn replace_parent(name: &str, parent: &str, qualified: &str) -> String {
    if name == parent {
        return qualified.to_owned();
    }
    let tail = if parent.is_empty() {
        name
    } else {
        name.strip_prefix(parent)
            .and_then(|tail| tail.strip_prefix('.'))
            .unwrap_or(name)
    };
    extract::qualify(".", qualified, tail)
}

fn method(
    assignment: Node<'_>,
    function: MemberFunction<'_>,
    name: &str,
    container: String,
    qualified_name: String,
    source: &str,
    language: &Language,
) -> Result<SyntaxSymbol, RiftError> {
    let item_range = extract::byte_range(assignment)?;
    let body_range = function
        .value
        .child_by_field_name("body")
        .map(extract::byte_range)
        .transpose()?;
    let declaration = Declaration {
        name: name.to_owned(),
        kind: EcmaScriptSymbolKind::Method.word(),
        kinds: EcmaScriptSymbolKind::Method.kinds(),
        visibility: None,
        body_range,
        documentation: Vec::new(),
        documentation_ranges: Vec::new(),
    };
    let signatures = extract::callable_signature(&declaration, item_range, source, language)
        .into_iter()
        .collect::<Vec<_>>();
    Ok(SyntaxSymbol {
        name: declaration.name,
        qualified_name,
        declaration_ordinal: 0,
        container: Some(container),
        kind: declaration.kind,
        syntax_element_kind: Some("assignment_expression"),
        kinds: declaration.kinds,
        visibility: None,
        range: item_range,
        item_range,
        name_range: Some(extract::byte_range(function.name)?),
        body_range,
        signatures: Arc::from(signatures),
        documentation: Arc::from([]),
        documentation_ranges: Vec::new(),
        module_path: None,
        python_overload: None,
    })
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
