//! Callable property assignments whose receiver has an unambiguous lexical binding.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_error::RiftError;
use rift_protocol::read::{Language, SymbolKind};
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

    fn owner(&self, receiver: Node<'_>, source: &str, prototype: bool) -> Option<usize> {
        let name = source.get(receiver.byte_range())?;
        if prototype && self.replaced.contains(name) {
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
    assignment: Node<'tree>,
    receiver: Node<'tree>,
    name: Node<'tree>,
    value: Node<'tree>,
    prototype: bool,
}

fn member_function<'tree>(
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
    let object = kinds.child(left, EcmaScriptGrammarField::Object)?;
    let prototype = object.kind_id() == kinds.member_expression;
    let receiver = if prototype {
        let property = kinds.child(object, EcmaScriptGrammarField::Property)?;
        if source.get(property.byte_range()) != Some("prototype") {
            return None;
        }
        kinds.child(object, EcmaScriptGrammarField::Object)?
    } else {
        // Literal CommonJS exports retain their separate binding and visibility checks.
        if kinds.export_target(left, source).is_some() {
            return None;
        }
        object
    };
    if receiver.kind_id() != kinds.identifier {
        return None;
    }
    let value = kinds.expression_value(kinds.child(assignment, EcmaScriptGrammarField::Right)?);
    kinds
        .function_values
        .contains(&value.kind_id())
        .then_some(MemberFunction {
            assignment,
            receiver,
            name,
            value,
            prototype,
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
        .filter_map(|&assignment| member_function(rules, assignment, source))
        .collect::<Vec<_>>();
    if functions.is_empty() {
        return Ok(());
    }
    let bindings = Bindings::new(source, facts, symbols);
    let mut declarations = DeclarationNames::new(functions[0].assignment, rules, facts, symbols);
    let mut containers = BTreeMap::new();
    // Assignments and declarations share the admitted syntax count. Renaming touches only
    // declarations in a callable's source interval, at most once per admitted ancestor depth.
    for function in functions {
        let assignment = function.assignment;
        let Some(owner) = bindings.owner(function.receiver, source, function.prototype) else {
            continue;
        };
        if (function.prototype && !matches!(symbols[owner].kind, "function" | "class"))
            || !matches!(symbols[owner].kind, "function" | "class" | "variable")
            || !declarations.unique(owner, symbols)
        {
            continue;
        }
        let container = symbols[owner].qualified_name.clone();
        let Some(name) = source.get(function.name.byte_range()) else {
            continue;
        };
        let qualified = extract::qualify(".", &container, name);
        let declaration = method_declaration(
            rules,
            source,
            function,
            name,
            !function.prototype && matches!(symbols[owner].kind, "function" | "class"),
        )?;
        let parent = qualification(rules, assignment, source, &containers);
        declarations
            .outlined
            .insert(extract::byte_range(function.value)?);
        declarations.rename(
            symbols,
            extract::byte_range(function.value)?,
            &parent,
            &qualified,
            rules,
        );
        containers.insert(assignment.id(), qualified.clone());
        *declarations.names.entry(qualified.clone()).or_default() += 1;
        symbols.push(method(
            rules,
            function,
            declaration,
            container,
            qualified,
            source,
            language,
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

struct DeclarationNames<'tree> {
    root: Node<'tree>,
    ordered: Vec<usize>,
    names: BTreeMap<String, usize>,
    known: BTreeSet<usize>,
    bodies: BTreeSet<ByteRange>,
    values: BTreeSet<ByteRange>,
    outlined: BTreeSet<ByteRange>,
}

impl<'tree> DeclarationNames<'tree> {
    fn new(
        mut root: Node<'tree>,
        rules: &EcmaScriptRules<'_>,
        facts: &StaticFacts,
        symbols: &[SyntaxSymbol],
    ) -> Self {
        // The source walk admitted every ancestor and declaration before this pass.
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let bodies = symbols
            .iter()
            .filter(|symbol| symbol.kinds.contains(&SymbolKind::Callable))
            .filter_map(|symbol| symbol.body_range)
            .collect::<BTreeSet<_>>();
        let values = facts
            .function_values()
            .iter()
            .filter(|(_, value)| bodies.contains(value))
            .map(|(range, _)| *range)
            .collect();
        let mut ordered = (0..symbols.len()).collect::<Vec<_>>();
        ordered.sort_by_key(|&index| symbols[index].range.start);
        let mut declarations = Self {
            root,
            ordered,
            names: BTreeMap::new(),
            known: BTreeSet::new(),
            bodies,
            values,
            outlined: BTreeSet::new(),
        };
        for (index, symbol) in symbols.iter().enumerate() {
            if declarations.identity_known(symbol, rules) {
                declarations.known.insert(index);
                *declarations
                    .names
                    .entry(symbol.qualified_name.clone())
                    .or_default() += 1;
            }
        }
        declarations
    }

    fn unique(&self, index: usize, symbols: &[SyntaxSymbol]) -> bool {
        self.known.contains(&index) && self.names.get(&symbols[index].qualified_name) == Some(&1)
    }

    fn identity_known(&self, symbol: &SyntaxSymbol, rules: &EcmaScriptRules<'_>) -> bool {
        let range = symbol.name_range.unwrap_or(symbol.item_range);
        let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end))
        else {
            return false;
        };
        let mut ancestor = self.root.descendant_for_byte_range(start, end);
        // Unnamed function values cannot supply a qualified owner. A checked member
        // assignment supplies its own receiver path, even inside an unnamed callback.
        while let Some(element) = ancestor {
            if rules.kinds.function_values.contains(&element.kind_id()) {
                let Ok(value) = extract::byte_range(element) else {
                    return false;
                };
                if self.outlined.contains(&value) {
                    return true;
                }
                let body = element
                    .child_by_field_name("body")
                    .and_then(|body| extract::byte_range(body).ok());
                if !self.values.contains(&value)
                    && !body.is_some_and(|body| self.bodies.contains(&body))
                {
                    return false;
                }
            }
            ancestor = element.parent();
        }
        true
    }

    fn rename(
        &mut self,
        symbols: &mut [SyntaxSymbol],
        range: ByteRange,
        parent: &str,
        qualified: &str,
        rules: &EcmaScriptRules<'_>,
    ) {
        let start = self
            .ordered
            .partition_point(|&index| symbols[index].range.start < range.start);
        let end = self
            .ordered
            .partition_point(|&index| symbols[index].range.start < range.end);
        for position in start..end {
            let index = self.ordered[position];
            let symbol = &mut symbols[index];
            if !range.encloses(symbol.item_range) {
                continue;
            }
            if self.known.remove(&index) {
                let count = self
                    .names
                    .get_mut(&symbol.qualified_name)
                    .expect("every known declaration has a name count");
                *count = count
                    .checked_sub(1)
                    .expect("a known declaration has a positive name count");
                if *count == 0 {
                    self.names.remove(&symbol.qualified_name);
                }
            }
            symbol.qualified_name = replace_parent(&symbol.qualified_name, parent, qualified);
            symbol.container = Some(symbol.container.as_deref().map_or_else(
                || qualified.to_owned(),
                |name| replace_parent(name, parent, qualified),
            ));
            if self.identity_known(symbol, rules) {
                self.known.insert(index);
                *self.names.entry(symbol.qualified_name.clone()).or_default() += 1;
            }
        }
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

fn method_declaration(
    rules: &EcmaScriptRules<'_>,
    source: &str,
    function: MemberFunction<'_>,
    name: &str,
    static_member: bool,
) -> Result<Declaration, RiftError> {
    let body_range = function
        .value
        .child_by_field_name("body")
        .map(extract::byte_range)
        .transpose()?;
    let mut kinds = EcmaScriptSymbolKind::Method.kinds();
    if static_member {
        kinds.push(SymbolKind::Static);
    }
    let (documentation, documentation_ranges) =
        rules.attached_jsdoc_node(statement(function, rules), source)?;
    Ok(Declaration {
        name: name.to_owned(),
        kind: EcmaScriptSymbolKind::Method.word(),
        kinds,
        visibility: None,
        body_range,
        documentation,
        documentation_ranges,
    })
}

fn method(
    rules: &EcmaScriptRules<'_>,
    function: MemberFunction<'_>,
    declaration: Declaration,
    container: String,
    qualified_name: String,
    source: &str,
    language: &Language,
) -> Result<SyntaxSymbol, RiftError> {
    let item_range = extract::byte_range(function.assignment)?;
    let start = rules
        .jsdoc_run_node(statement(function, rules), source)
        .last()
        .copied()
        .map(extract::byte_range)
        .transpose()?
        .map_or(item_range.start, |range| range.start);
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
        range: ByteRange {
            start,
            end: item_range.end,
        },
        item_range,
        name_range: Some(extract::byte_range(function.name)?),
        body_range: declaration.body_range,
        signatures: Arc::from(signatures),
        documentation: Arc::from(declaration.documentation),
        documentation_ranges: declaration.documentation_ranges,
        module_path: None,
        python_overload: None,
    })
}

fn statement<'tree>(function: MemberFunction<'tree>, rules: &EcmaScriptRules<'_>) -> Node<'tree> {
    function
        .assignment
        .parent()
        .filter(|parent| parent.kind_id() == rules.kinds.expression_statement)
        .unwrap_or(function.assignment)
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
