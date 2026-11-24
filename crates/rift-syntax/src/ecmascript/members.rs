//! Callable property assignments whose receiver has an unambiguous lexical binding.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_error::RiftError;
use rift_protocol::read::{Language, SymbolKind};
use tree_sitter::Node;

use super::{EcmaScriptGrammarField, EcmaScriptRules, EcmaScriptSymbolKind};
use crate::extract::{self, Declaration, GrammarRules, Visited, Visits};
use crate::{
    ByteRange, StaticBinding, StaticBindingKind, StaticFacts, StaticMember, StaticMemberAlias,
    SyntaxSymbol,
};

struct Bindings<'source> {
    scopes: BTreeMap<(ByteRange, &'source str), Option<&'source StaticBinding>>,
    symbols: BTreeMap<ByteRange, Option<usize>>,
    members: BTreeMap<ByteRange, &'source StaticMember>,
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
            members,
            replaced,
            blocked: facts.unsupported_bindings().iter().copied().collect(),
        }
    }

    fn owner(
        &self,
        function: MemberFunction<'_, '_>,
        source: &str,
        rules: &EcmaScriptRules<'_>,
        visits: &Visits<'_>,
        symbols: &[SyntaxSymbol],
    ) -> Option<usize> {
        let receiver = function.receiver;
        let prototype = function.prototype;
        let name = source.get(receiver.byte_range())?;
        if prototype && self.replaced.contains(name) {
            return None;
        }
        let (binding, owner) = self.lexical_owner(function.visited, name)?;
        if prototype && symbols[owner].kind == "variable" {
            direct_constructor_initializer(rules, visits, binding, function)?;
        }
        Some(owner)
    }

    fn lexical_owner(
        &self,
        visited: Visited<'_, '_>,
        name: &str,
    ) -> Option<(&'source StaticBinding, usize)> {
        let mut ancestor = Some(visited);
        // The shared walk has admitted the source depth before this lookup.
        while let Some(visited) = ancestor {
            let scope = visited.syntax_element();
            if self.blocked.contains(&extract::byte_range(scope).ok()?) {
                return None;
            }
            if let Some(binding) = self.scopes.get(&(extract::byte_range(scope).ok()?, name)) {
                let binding = (*binding)?;
                if binding.kind != StaticBindingKind::Local {
                    return None;
                }
                let owner = self.symbols.get(&binding.name).copied().flatten()?;
                return Some((binding, owner));
            }
            ancestor = visited.parent();
        }
        None
    }
}

#[derive(Clone, Copy)]
struct MemberFunction<'walk, 'tree> {
    visited: Visited<'walk, 'tree>,
    assignment: Node<'tree>,
    receiver: Node<'tree>,
    name: Node<'tree>,
    value: Node<'tree>,
    prototype: bool,
    /// `this.name = value` inside a class method or a constructor function (D38).
    this_member: bool,
}

fn member_function<'walk, 'tree>(
    rules: &EcmaScriptRules<'_>,
    visited: Visited<'walk, 'tree>,
    source: &str,
) -> Option<MemberFunction<'walk, 'tree>> {
    let assignment = visited.syntax_element();
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
    if object.kind() == "this" {
        // Any value declares the member; a function value makes it a method.
        let value = kinds.expression_value(kinds.child(assignment, EcmaScriptGrammarField::Right)?);
        return Some(MemberFunction {
            visited,
            assignment,
            receiver: object,
            name,
            value,
            prototype: false,
            this_member: true,
        });
    }
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
            visited,
            assignment,
            receiver,
            name,
            value,
            prototype,
            this_member: false,
        })
}

/// A prototype receiver initialized by an ordinary function earlier in the same statement list.
fn direct_constructor_initializer(
    rules: &EcmaScriptRules<'_>,
    visits: &Visits<'_>,
    binding: &StaticBinding,
    function: MemberFunction<'_, '_>,
) -> Option<()> {
    let name = visits.containing(binding.name)?.syntax_element();
    let declaration = name.parent()?;
    let kinds = rules.kinds;
    if declaration.kind() != "variable_declarator"
        || kinds.child(declaration, EcmaScriptGrammarField::Name) != Some(name)
        || declaration.has_error()
    {
        return None;
    }
    let value = kinds.child(declaration, EcmaScriptGrammarField::Value)?;
    if value.kind() != "function_expression" {
        return None;
    }
    let mut cursor = value.walk();
    let ordinary = value
        .children(&mut cursor)
        .find(|child| !child.is_extra())
        .is_some_and(|child| child.kind() == "function" && !child.is_missing());
    let declaration = declaration.parent()?;
    let assignment = function.assignment.parent()?;
    (ordinary
        && kinds.is_declaration_statement(declaration)
        && assignment.kind() == "expression_statement"
        && declaration.parent()? == assignment.parent()?
        && declaration.end_byte() <= assignment.start_byte())
    .then_some(())
}

/// The declaration `this` names at a `this.name = value` assignment: the class whose
/// method or constructor holds it, or the function declaration that holds it. Arrow
/// functions keep the surrounding `this`; any other function binds its own, and then the
/// assignment declares nothing.
fn this_owner(
    function: MemberFunction<'_, '_>,
    symbols: &BTreeMap<ByteRange, Option<usize>>,
) -> Option<usize> {
    let mut ancestor = function.visited.parent();
    // The shared walk has admitted the source depth before this lookup.
    while let Some(visited) = ancestor {
        let element = visited.syntax_element();
        // An arrow function keeps the surrounding `this`, so the walk passes it.
        match element.kind() {
            "method_definition" => {
                let class = visited
                    .parent()
                    .filter(|body| body.syntax_element().kind() == "class_body")?
                    .parent()?
                    .syntax_element();
                let name = class.child_by_field_name("name")?;
                return symbols
                    .get(&extract::byte_range(name).ok()?)
                    .copied()
                    .flatten();
            }
            "function_declaration" => {
                let name = element.child_by_field_name("name")?;
                return symbols
                    .get(&extract::byte_range(name).ok()?)
                    .copied()
                    .flatten();
            }
            "function_expression"
            | "function"
            | "generator_function"
            | "generator_function_declaration"
            | "class_static_block"
            | "program" => {
                return None;
            }
            _ => {}
        }
        ancestor = visited.parent();
    }
    None
}

pub(super) fn record<'tree>(
    rules: &EcmaScriptRules<'_>,
    source: &str,
    language: &Language,
    facts: &StaticFacts,
    visits: &Visits<'tree>,
    assignments: &[Node<'tree>],
    symbols: &mut Vec<SyntaxSymbol>,
) -> Result<Vec<StaticMemberAlias>, RiftError> {
    let functions = assignments
        .iter()
        .filter_map(|&assignment| {
            let visited = visits.containing(extract::byte_range(assignment).ok()?)?;
            (visited.syntax_element() == assignment)
                .then(|| member_function(rules, visited, source))
                .flatten()
        })
        .collect::<Vec<_>>();
    if functions.is_empty() {
        return Ok(Vec::new());
    }
    let bindings = Bindings::new(source, facts, symbols);
    let mut declarations = DeclarationNames::new(visits, rules, facts, symbols);
    let mut containers = BTreeMap::new();
    let mut aliases = Vec::new();
    // Assignments and declarations share the admitted syntax count. Renaming touches only
    // declarations in a callable's source interval, at most once per admitted ancestor depth.
    for function in functions {
        let assignment = function.assignment;
        let owner = if function.this_member {
            this_owner(function, &bindings.symbols)
        } else {
            bindings.owner(function, source, rules, visits, symbols)
        };
        let Some(owner) = owner else {
            continue;
        };
        if (function.this_member && !matches!(symbols[owner].kind, "function" | "class"))
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
            !function.prototype
                && !function.this_member
                && matches!(symbols[owner].kind, "function" | "class"),
        )?;
        let parent = qualification(rules, function.visited, source, &containers);
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
        let method = method(
            rules,
            function,
            declaration,
            container,
            qualified,
            source,
            language,
        )?;
        if let Some(alias) = assignment_alias(rules, source, &bindings, function, owner) {
            aliases.push((alias, owner, symbols.len()));
        }
        symbols.push(method);
    }
    record_aliases(source, facts, &bindings, visits, &aliases, symbols)
}

/// The outer assignment in `object.alias = object.method = function ...`.
fn assignment_alias<'walk, 'tree>(
    rules: &EcmaScriptRules<'_>,
    source: &str,
    bindings: &Bindings<'_>,
    function: MemberFunction<'walk, 'tree>,
    owner: usize,
) -> Option<MemberFunction<'walk, 'tree>> {
    let kinds = rules.kinds;
    if function.prototype
        || function.this_member
        || kinds.child(function.assignment, EcmaScriptGrammarField::Right) != Some(function.value)
    {
        return None;
    }
    let visited = function.visited.parent()?;
    let assignment = visited.syntax_element();
    if assignment.kind_id() != kinds.assignment_expression
        || assignment.has_error()
        || kinds.child(assignment, EcmaScriptGrammarField::Right) != Some(function.assignment)
    {
        return None;
    }
    let target = kinds.child(assignment, EcmaScriptGrammarField::Left)?;
    let receiver = kinds.child(target, EcmaScriptGrammarField::Object)?;
    let name = kinds.child(target, EcmaScriptGrammarField::Property)?;
    if target.kind_id() != kinds.member_expression
        || receiver.kind_id() != kinds.identifier
        || name.kind_id() != kinds.property_identifier
        || source.get(name.byte_range())? == source.get(function.name.byte_range())?
    {
        return None;
    }
    let (outer_binding, outer) =
        bindings.lexical_owner(visited, source.get(receiver.byte_range())?)?;
    let (inner_binding, _) = bindings.lexical_owner(
        function.visited,
        source.get(function.receiver.byte_range())?,
    )?;
    if outer != owner
        || outer_binding.name != inner_binding.name
        || outer_binding.available_from > extract::byte_range(receiver).ok()?.start
    {
        return None;
    }
    Some(MemberFunction {
        visited,
        assignment,
        receiver,
        name,
        ..function
    })
}

fn record_aliases(
    source: &str,
    facts: &StaticFacts,
    bindings: &Bindings<'_>,
    visits: &Visits<'_>,
    candidates: &[(MemberFunction<'_, '_>, usize, usize)],
    symbols: &mut Vec<SyntaxSymbol>,
) -> Result<Vec<StaticMemberAlias>, RiftError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let receivers = candidates
        .iter()
        .filter_map(|(alias, _, _)| source.get(alias.receiver.byte_range()))
        .collect::<BTreeSet<_>>();
    let mut unknown = BTreeSet::new();
    let mut writes = BTreeMap::<(usize, Option<&str>), u8>::new();
    for &target in facts.assignments() {
        let member = bindings.members.get(&target);
        let receiver = member.map_or(target, |member| member.receiver);
        let Some(name) = text(source, receiver).filter(|name| receivers.contains(name)) else {
            continue;
        };
        let Some((_, owner)) = visits
            .containing(receiver)
            .and_then(|visited| bindings.lexical_owner(visited, name))
        else {
            unknown.insert(name);
            continue;
        };
        let member = member
            .and_then(|member| member.member)
            .and_then(|member| text(source, member));
        let count = writes.entry((owner, member)).or_default();
        *count = count.saturating_add(1);
    }
    let mut aliases = Vec::new();
    let mut declarations = Vec::new();
    let names = symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    for &(alias, owner, inner) in candidates {
        let Some(name) = source.get(alias.name.byte_range()) else {
            continue;
        };
        let target = &symbols[inner];
        let qualified_name = extract::qualify(".", &symbols[owner].qualified_name, name);
        if unknown.contains(source.get(alias.receiver.byte_range()).unwrap_or_default())
            || target.body_range.is_none()
            || writes.get(&(owner, Some(name))) != Some(&1)
            || writes.get(&(owner, Some(target.name.as_str()))) != Some(&1)
            || writes.contains_key(&(owner, None))
            || names.contains(qualified_name.as_str())
        {
            continue;
        }
        let Some(target_name) = target.name_range else {
            continue;
        };
        let range = extract::byte_range(alias.assignment)?;
        let alias = extract::byte_range(alias.name)?;
        declarations.push(SyntaxSymbol {
            name: name.to_owned(),
            qualified_name,
            range,
            item_range: range,
            name_range: Some(alias),
            body_range: None,
            signatures: Arc::default(),
            documentation: Arc::default(),
            documentation_ranges: Vec::new(),
            ..target.clone()
        });
        aliases.push(StaticMemberAlias {
            name: alias,
            target: target_name,
        });
    }
    symbols.extend(declarations);
    aliases.sort_by_key(|alias| alias.name);
    Ok(aliases)
}

fn qualification(
    rules: &EcmaScriptRules<'_>,
    assignment: Visited<'_, '_>,
    source: &str,
    containers: &BTreeMap<usize, String>,
) -> String {
    let mut parts = Vec::new();
    let mut ancestor = assignment.parent();
    // Parent traversal is bounded by the shared walk's admitted depth.
    while let Some(visited) = ancestor {
        let element = visited.syntax_element();
        if let Some(container) = containers.get(&element.id()) {
            parts.push(container.clone());
            break;
        }
        if let Some(name) = rules.container_name(element, source) {
            parts.push(name);
        }
        ancestor = visited.parent();
    }
    parts.reverse();
    parts.join(".")
}

struct DeclarationNames<'walk, 'tree> {
    visits: &'walk Visits<'tree>,
    ordered: Vec<usize>,
    names: BTreeMap<String, usize>,
    known: BTreeSet<usize>,
    bodies: BTreeSet<ByteRange>,
    values: BTreeSet<ByteRange>,
    outlined: BTreeSet<ByteRange>,
}

impl<'walk, 'tree> DeclarationNames<'walk, 'tree> {
    fn new(
        visits: &'walk Visits<'tree>,
        rules: &EcmaScriptRules<'_>,
        facts: &StaticFacts,
        symbols: &[SyntaxSymbol],
    ) -> Self {
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
            visits,
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
        let mut ancestor = self.visits.containing(range);
        // Unnamed function values cannot supply a qualified owner. A checked member
        // assignment supplies its own receiver path, even inside an unnamed callback.
        while let Some(visited) = ancestor {
            let element = visited.syntax_element();
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
            ancestor = visited.parent();
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
    function: MemberFunction<'_, '_>,
    name: &str,
    static_member: bool,
) -> Result<Declaration, RiftError> {
    // A function value declares a method; any other value a field, whose body is the
    // object literal it holds.
    let callable = rules
        .kinds
        .function_values
        .contains(&function.value.kind_id());
    let symbol_kind = if callable {
        EcmaScriptSymbolKind::Method
    } else {
        EcmaScriptSymbolKind::Field
    };
    let body_range = if callable {
        function
            .value
            .child_by_field_name("body")
            .map(extract::byte_range)
            .transpose()?
    } else if function.value.kind_id() == rules.kinds.object_literal {
        Some(extract::byte_range(function.value)?)
    } else {
        None
    };
    let mut kinds = symbol_kind.kinds();
    if static_member {
        kinds.push(SymbolKind::Static);
    }
    if rules.exports.declared.contains(&function.assignment.id()) {
        kinds.push(SymbolKind::Public);
    }
    let (documentation, documentation_ranges) =
        rules.attached_jsdoc(statement(function, rules), source)?;
    Ok(Declaration {
        name: name.to_owned(),
        kind: symbol_kind.word(),
        kinds,
        visibility: None,
        body_range,
        documentation,
        documentation_ranges,
    })
}

fn method(
    rules: &EcmaScriptRules<'_>,
    function: MemberFunction<'_, '_>,
    declaration: Declaration,
    container: String,
    qualified_name: String,
    source: &str,
    language: &Language,
) -> Result<SyntaxSymbol, RiftError> {
    let item_range = extract::byte_range(function.assignment)?;
    let start = rules
        .jsdoc_run(statement(function, rules), source)
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

fn statement<'walk, 'tree>(
    function: MemberFunction<'walk, 'tree>,
    rules: &EcmaScriptRules<'_>,
) -> Visited<'walk, 'tree> {
    function
        .visited
        .parent()
        .filter(|parent| parent.syntax_element().kind_id() == rules.kinds.expression_statement)
        .unwrap_or(function.visited)
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
