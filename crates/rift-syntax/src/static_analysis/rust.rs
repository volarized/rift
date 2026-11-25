mod imports;
mod macro_scopes;
mod macros;
mod references;

pub(super) use macros::Macros;

use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBindingKind, StaticCallee, StaticFactsParts, StaticInherentImpl, binding, call,
    enclosing, field, identifier, kind,
};
use crate::ByteRange;
use crate::extract::{Visited, byte_range};

/// Most names one unread place can bind; a place with more costs its scope.
const UNREAD_NAMES_MAX: usize = 64;

/// Most unread places of one file that cost less than their scope.
const UNREAD_PLACES_MAX: usize = 1024;

pub(super) struct Kinds {
    identifier: u16,
    field_identifier: u16,
    type_identifier: u16,
    generic_type: u16,
    reference_type: u16,
    array_type: u16,
    tuple_type: u16,
    unit_type: u16,
    self_parameter: u16,
    self_value: u16,
    scoped_identifier: u16,
    scoped_type_identifier: u16,
    call: u16,
    generic_function: u16,
    field_expression: u16,
    field_declaration: u16,
    value_items: [u16; 2],
    struct_item: u16,
    tuple_fields: u16,
    foreign_item: u16,
    function_type: u16,
    struct_expression: u16,
    field_initializers: [u16; 2],
    struct_pattern: u16,
    use_declaration: u16,
    use_alias: u16,
    use_list: [u16; 3],
    let_declaration: u16,
    value_path_statements: [u16; 2],
    local_value_items: [u16; 2],
    local_value_struct: u16,
    ordered_value_fields: u16,
    local_type_items: [u16; 3],
    named_value_patterns: [u16; 2],
    parameter: u16,
    closure_parameters: u16,
    parameters: u16,
    conditional: [u16; 4],
    attribute: u16,
    inner_attribute: u16,
    impl_item: u16,
    trait_item: u16,
    type_item: u16,
    block_type_items: [u16; 5],
    type_parameter: u16,
    where_predicate: u16,
    non_method_bounds: [u16; 2],
    bounds: u16,
    edge_macro: u16,
    macro_definition: u16,
    macro_name: u16,
    assignment: [u16; 2],
    scopes: [u16; 3],
    functions: [u16; 3],
    function: u16,
    value: u16,
    member: u16,
    argument: u16,
    arguments: u16,
    name: u16,
    alias: u16,
    path: u16,
    pattern: u16,
    match_pattern: u16,
    condition: u16,
    type_arguments: u16,
    named_patterns: [u16; 3],
    field_pattern: u16,
    shorthand_field: u16,
    value_type: u16,
    return_type: u16,
    left: u16,
    body: u16,
}

pub(super) fn kinds() -> &'static Kinds {
    static KINDS: OnceLock<Kinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = crate::rust::rust_grammar();
        let id = |name| kind(&grammar, name);
        let field = |name| field(&grammar, name);
        Kinds {
            identifier: id("identifier"),
            field_identifier: id("field_identifier"),
            type_identifier: id("type_identifier"),
            generic_type: id("generic_type"),
            reference_type: id("reference_type"),
            array_type: id("array_type"),
            tuple_type: id("tuple_type"),
            unit_type: id("unit_type"),
            self_parameter: id("self_parameter"),
            self_value: id("self"),
            scoped_identifier: id("scoped_identifier"),
            scoped_type_identifier: id("scoped_type_identifier"),
            call: id("call_expression"),
            generic_function: id("generic_function"),
            field_expression: id("field_expression"),
            field_declaration: id("field_declaration"),
            value_items: [id("const_item"), id("static_item")],
            struct_item: id("struct_item"),
            tuple_fields: id("ordered_field_declaration_list"),
            foreign_item: id("foreign_mod_item"),
            function_type: id("function_type"),
            struct_expression: id("struct_expression"),
            field_initializers: [id("field_initializer"), id("shorthand_field_initializer")],
            struct_pattern: id("struct_pattern"),
            use_declaration: id("use_declaration"),
            use_alias: id("use_as_clause"),
            use_list: [id("use_list"), id("scoped_use_list"), id("use_wildcard")],
            let_declaration: id("let_declaration"),
            value_path_statements: [id("return_expression"), id("expression_statement")],
            local_value_items: [id("const_item"), id("static_item")],
            local_value_struct: id("struct_item"),
            ordered_value_fields: id("ordered_field_declaration_list"),
            local_type_items: [id("enum_item"), id("union_item"), id("mod_item")],
            named_value_patterns: [id("tuple_struct_pattern"), id("struct_pattern")],
            parameter: id("parameter"),
            closure_parameters: id("closure_parameters"),
            parameters: id("parameters"),
            conditional: [
                id("let_condition"),
                id("match_arm"),
                id("for_expression"),
                id("if_expression"),
            ],
            attribute: id("attribute_item"),
            inner_attribute: id("inner_attribute_item"),
            impl_item: id("impl_item"),
            trait_item: id("trait_item"),
            type_item: id("type_item"),
            block_type_items: [
                id("struct_item"),
                id("enum_item"),
                id("union_item"),
                id("trait_item"),
                id("type_item"),
            ],
            type_parameter: id("type_parameter"),
            where_predicate: id("where_predicate"),
            non_method_bounds: [id("lifetime"), id("removed_trait_bound")],
            bounds: field("bounds"),
            edge_macro: id("macro_invocation"),
            macro_definition: id("macro_definition"),
            macro_name: field("macro"),
            assignment: [id("assignment_expression"), id("compound_assignment_expr")],
            scopes: [id("source_file"), id("block"), id("declaration_list")],
            functions: [
                id("function_item"),
                id("function_signature_item"),
                id("closure_expression"),
            ],
            function: field("function"),
            value: field("value"),
            member: field("field"),
            argument: field("argument"),
            arguments: field("arguments"),
            name: field("name"),
            alias: field("alias"),
            path: field("path"),
            pattern: field("pattern"),
            match_pattern: id("match_pattern"),
            condition: field("condition"),
            type_arguments: field("type_arguments"),
            named_patterns: [
                id("struct_pattern"),
                id("tuple_struct_pattern"),
                id("generic_pattern"),
            ],
            field_pattern: id("field_pattern"),
            shorthand_field: id("shorthand_field_identifier"),
            value_type: field("type"),
            return_type: field("return_type"),
            left: field("left"),
            body: field("body"),
        }
    })
}

impl Kinds {
    pub(super) fn record<'source>(
        &self,
        visited: Visited<'_, '_>,
        source: &'source str,
        facts: &mut StaticFactsParts,
        budget: &mut rift_core::LoopBudget,
        macros: &mut Macros<'source>,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let mut ancestor = Some(visited);
        // The shared walk already bounds parent depth. Opaque macro contents and
        // parser recovery subtrees cannot establish executable facts.
        while let Some(current) = ancestor {
            let element = current.syntax_element();
            if (element.kind_id() == self.macro_definition && element != syntax_element)
                || element.is_error()
                || element.is_missing()
            {
                return Ok(());
            }
            ancestor = current.parent();
        }
        let id = syntax_element.kind_id();
        if id == self.macro_definition {
            self.record_macro_definition(visited, source, macros)?;
            return self.record_textual_macro_definition(visited, facts);
        }
        self.record_reference(visited, facts)?;
        if self.block_type_items.contains(&id) {
            self.record_block_type(visited, facts)?;
        }
        if id == self.call {
            self.record_call(syntax_element, facts)?;
        } else if id == self.struct_expression {
            if let Some(callee) = syntax_element
                .child_by_field_id(self.name)
                .and_then(|name| self.named_type(name))
            {
                let shape = StaticCallee::Qualified;
                facts.calls.push(call(syntax_element, callee, shape)?);
            }
        } else if id == self.use_declaration {
            self.record_import(visited, facts, budget)?;
        } else if id == self.self_parameter
            || (id == self.parameter
                && syntax_element
                    .child_by_field_id(self.pattern)
                    .is_some_and(|pattern| pattern.kind_id() == self.self_value))
        {
            self.record_receiver(visited, source, facts)?;
        } else if id == self.let_declaration || id == self.parameter {
            self.record_value(visited, facts)?;
        } else if id == self.identifier
            && visited
                .parent()
                .is_some_and(|parent| parent.syntax_element().kind_id() == self.closure_parameters)
        {
            self.record_closure_argument(visited, facts)?;
        } else if id == self.functions[0] {
            self.record_item_binding(syntax_element, visited, facts)?;
            self.record_return(syntax_element, facts)?;
        } else if id == self.functions[1] {
            self.record_return(syntax_element, facts)?;
            self.record_named_value_binding(visited, facts)?;
        } else if self.value_items.contains(&id) || self.value_struct(syntax_element) {
            self.record_named_value_binding(visited, facts)?;
        } else if id == self.field_declaration {
            self.record_function_field(syntax_element, facts)?;
        } else if self.assignment.contains(&id) {
            self.record_mutation(syntax_element, facts)?;
        } else if id == self.edge_macro {
            facts.unsupported_bindings.push(byte_range(syntax_element)?);
            self.record_macro_binding(visited, source, macros, facts)?;
            self.record_macro_invocation(visited, source, facts)?;
        } else if id == self.local_type_items[2]
            && syntax_element.child_by_field_id(self.body).is_none()
        {
            // A file-backed module can carry a macro_use inner attribute.
            self.record_macro_scope_barrier(visited, facts)?;
        } else if id == self.impl_item {
            self.record_impl(syntax_element, facts)?;
        } else if id == self.type_item {
            if let Some(name) = syntax_element.child_by_field_id(self.name)
                && let Some(value) = syntax_element.child_by_field_id(self.value_type)
                && !name.is_missing()
                && !value.is_missing()
            {
                facts
                    .declared_types
                    .push((byte_range(name)?, byte_range(value)?));
            }
        } else if id == self.type_parameter || id == self.where_predicate {
            self.record_bounds(visited, facts)?;
        } else if id == self.attribute || id == self.inner_attribute {
            self.record_attribute(visited, source, facts)?;
        } else if self.conditional[..3].contains(&id) {
            let scope = if id == self.conditional[0] {
                enclosing(visited, &self.conditional[3..])
                    .map_or(syntax_element, Visited::syntax_element)
            } else {
                syntax_element
            };
            self.record_pattern(syntax_element, scope, facts)?;
        } else if id == self.parameters || id == self.closure_parameters {
            self.record_patterns(visited, facts)?;
        }
        Ok(())
    }

    /// Records a place the parser could not read.
    ///
    /// The place costs the calls of the smallest syntax element the parser closed around it,
    /// and every name written in the unread text: it can bind one in the enclosing scope. The
    /// declarations and statements beside that element keep their structure when the unread
    /// text closes every delimiter it opens. A place that stands among the statements of a
    /// file or block, or that leaves a delimiter open, can tear its neighbors apart and costs
    /// the whole scope.
    pub(super) fn record_error(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        unread: &mut Vec<ByteRange>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let mut ancestor = visited.parent();
        let mut scope = None;
        while let Some(current) = ancestor {
            let element = current.syntax_element();
            if element.kind_id() == self.macro_definition || element.is_error() {
                return Ok(());
            }
            if scope.is_none() && self.scopes.contains(&element.kind_id()) {
                scope = Some(element);
            }
            ancestor = current.parent();
        }
        let Some((scope, parent)) = scope.zip(visited.parent().map(Visited::syntax_element)) else {
            return Ok(());
        };
        let text = source.get(element.byte_range()).unwrap_or_default();
        // Only a declaration list admits nothing but whole items around the place.
        let place = if !self.scopes.contains(&parent.kind_id()) {
            Some(parent)
        } else if parent.kind_id() == self.scopes[2] && element.is_error() {
            Some(element)
        } else {
            None
        };
        let names = identifiers(text)
            .take(UNREAD_NAMES_MAX + 1)
            .collect::<Vec<_>>();
        let Some(place) = place.filter(|place| {
            !place.byte_range().is_empty()
                && closed(text)
                && names.len() <= UNREAD_NAMES_MAX
                && unread.len() < UNREAD_PLACES_MAX
        }) else {
            facts.unsupported_bindings.push(byte_range(scope)?);
            return Ok(());
        };
        let place = byte_range(place)?;
        facts.unsupported_bindings.push(place);
        unread.push(place);
        let scope_start = byte_range(scope)?.start;
        let start = byte_range(element)?.start;
        for name in names {
            let name = ByteRange {
                start: start + name.start as u64,
                end: start + name.end as u64,
            };
            // An item is in its whole scope, a pattern in what follows it.
            for available_from in [scope_start, place.end] {
                facts.bindings.push(super::StaticBinding {
                    name,
                    scope: byte_range(scope)?,
                    available_from,
                    kind: StaticBindingKind::Shadow,
                });
            }
        }
        Ok(())
    }

    /// Takes the type of every binding away whose own text holds an unread place or whose
    /// initializer starts in one: the parser read something else there.
    pub(super) fn close_unread(mut unread: Vec<ByteRange>, facts: &mut StaticFactsParts) {
        if unread.is_empty() {
            return;
        }
        unread.sort_unstable();
        // Every place overlapping an earlier one extends it, so a search by start finds it.
        let mut places: Vec<ByteRange> = Vec::with_capacity(unread.len());
        for place in unread {
            match places.last_mut() {
                Some(last) if place.start < last.end => last.end = last.end.max(place.end),
                _ => places.push(place),
            }
        }
        let holds = |position: u64| {
            let next = places.partition_point(|place| place.start <= position);
            next.checked_sub(1)
                .is_some_and(|index| position < places[index].end)
        };
        for binding in &mut facts.bindings {
            let typed = match binding.kind {
                StaticBindingKind::RustValue { initializer } => holds(initializer.start),
                StaticBindingKind::RustReceiver { .. } => false,
                _ => continue,
            };
            if typed || holds(binding.name.start) {
                binding.kind = StaticBindingKind::Shadow;
            }
        }
    }

    fn record_attribute(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let name = syntax_element
            .named_child(0)
            .and_then(|attribute| attribute.named_child(0));
        let name = name.and_then(|name| source.get(name.byte_range()));
        let conditional_attribute = name == Some("cfg")
            || (name == Some("cfg_attr") && self.contains_cfg(syntax_element, source));
        let opaque_macro_attribute = !matches!(
            name,
            Some("allow" | "cfg" | "deny" | "doc" | "forbid" | "macro_export" | "warn")
        );
        if !conditional_attribute && !opaque_macro_attribute {
            return Ok(());
        }
        let parent = visited.parent().map(Visited::syntax_element);
        let conditional = if syntax_element.kind_id() == self.inner_attribute
            || parent.is_some_and(|parent| parent.kind_id() == self.field_pattern)
        {
            parent
        } else {
            // Consecutive attributes are source-ordered siblings. A cfg at the end of
            // a list owns the next item only when that item exists.
            let mut next = syntax_element.next_named_sibling();
            while let Some(sibling) = next {
                if sibling.kind_id() != self.attribute && !sibling.is_extra() {
                    break;
                }
                next = sibling.next_named_sibling();
            }
            next
        };
        if let Some(item) = conditional {
            if conditional_attribute {
                facts.conditional_items.push(super::StaticConditionalItem {
                    range: byte_range(item)?,
                    attribute: byte_range(syntax_element)?,
                });
            }
            if opaque_macro_attribute {
                self.record_macro_attribute_barrier(visited, item, facts)?;
            }
        }
        Ok(())
    }

    fn contains_cfg(&self, element: Node<'_>, source: &str) -> bool {
        let mut pending = vec![element];
        let mut cursor = element.walk();
        while let Some(element) = pending.pop() {
            if element.kind_id() == self.identifier
                && source.get(element.byte_range()) == Some("cfg")
            {
                return true;
            }
            pending.extend(element.named_children(&mut cursor));
        }
        false
    }

    fn record_call(
        &self,
        syntax_element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(expression) = syntax_element
            .child_by_field_id(self.function)
            .filter(|syntax_element| !syntax_element.is_missing())
        else {
            return Ok(());
        };
        let callee = if expression.kind_id() == self.generic_function {
            expression
                .child_by_field_id(self.function)
                .unwrap_or(expression)
        } else {
            expression
        };
        let shape = if callee.kind_id() == self.identifier {
            StaticCallee::Direct
        } else if callee.kind_id() == self.scoped_identifier {
            StaticCallee::Qualified
        } else {
            self.member(callee)?
        };
        facts.calls.push(call(syntax_element, callee, shape)?);
        if let Some(arguments) = syntax_element.child_by_field_id(self.arguments) {
            let mut cursor = arguments.walk();
            let arguments = arguments
                .named_children(&mut cursor)
                .filter(|argument| !argument.is_extra())
                .collect::<Vec<_>>();
            if arguments
                .iter()
                .any(|argument| argument.kind_id() == self.functions[2])
            {
                let ranges = arguments
                    .iter()
                    .map(|argument| byte_range(*argument))
                    .collect::<Result<Vec<_>, _>>()?;
                facts
                    .closure_calls
                    .push((byte_range(syntax_element)?, ranges));
            }
        }
        if shape == StaticCallee::Qualified
            && let Some(arguments) = syntax_element.child_by_field_id(self.arguments)
        {
            let mut cursor = arguments.walk();
            if let Some(first) = arguments
                .named_children(&mut cursor)
                .find(|argument| !argument.is_extra())
                .filter(|argument| !argument.is_missing())
            {
                facts
                    .receiver_arguments
                    .push((byte_range(syntax_element)?, byte_range(first)?));
            }
        }
        Ok(())
    }

    fn member(&self, syntax_element: Node<'_>) -> Result<StaticCallee, RiftError> {
        if syntax_element.kind_id() != self.field_expression {
            return Ok(StaticCallee::Unsupported);
        }
        let receiver = syntax_element
            .child_by_field_id(self.value)
            .and_then(|syntax_element| {
                (!syntax_element.is_missing()
                    && [
                        self.identifier,
                        self.self_value,
                        self.field_expression,
                        self.call,
                        self.struct_expression,
                    ]
                    .contains(&syntax_element.kind_id()))
                .then_some(syntax_element)
            });
        let member = syntax_element
            .child_by_field_id(self.member)
            .and_then(|syntax_element| identifier(syntax_element, self.field_identifier));
        match receiver.zip(member) {
            Some((receiver, member)) => Ok(StaticCallee::Member {
                receiver: byte_range(receiver)?,
                member: byte_range(member)?,
            }),
            None => Ok(StaticCallee::Unsupported),
        }
    }

    fn record_value(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let is_let = syntax_element.kind_id() == self.let_declaration;
        let scopes = if is_let {
            &self.scopes
        } else {
            &self.functions
        };
        let Some(scope) = enclosing(visited, scopes).map(Visited::syntax_element) else {
            return Ok(());
        };
        let Some(name) = syntax_element
            .child_by_field_id(self.pattern)
            .and_then(|syntax_element| identifier(syntax_element, self.identifier))
        else {
            self.record_pattern(syntax_element, scope, facts)?;
            return Ok(());
        };
        let value_type = syntax_element
            .child_by_field_id(self.value_type)
            .or_else(|| self.struct_value(syntax_element))
            .and_then(|value_type| self.named_type(value_type));
        let closure = syntax_element
            .child_by_field_id(self.value)
            .is_some_and(|value| value.kind_id() == self.functions[2]);
        let initializer = syntax_element
            .child_by_field_id(self.value)
            .filter(|value| {
                [
                    self.call,
                    self.identifier,
                    self.scoped_identifier,
                    self.self_value,
                    self.type_identifier,
                    self.field_expression,
                ]
                .contains(&value.kind_id())
            });
        let kind = match (value_type, closure, initializer) {
            (Some(value_type), _, _) => StaticBindingKind::RustReceiver {
                type_path: byte_range(value_type)?,
            },
            (_, true, _) => StaticBindingKind::Local,
            (_, _, Some(initializer)) => StaticBindingKind::RustValue {
                initializer: byte_range(initializer)?,
            },
            _ => StaticBindingKind::Shadow,
        };
        let available_from = if is_let {
            byte_range(syntax_element)?.end
        } else {
            byte_range(scope)?.start
        };
        facts
            .bindings
            .push(binding(name, scope, available_from, kind)?);
        Ok(())
    }

    fn record_receiver(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let Some(scope) = enclosing(visited, &self.functions) else {
            return Ok(());
        };
        let Some(implementation) =
            enclosing(scope, &[self.impl_item, self.trait_item]).map(Visited::syntax_element)
        else {
            return Ok(());
        };
        let scope = scope.syntax_element();
        let Some(value_type) = implementation
            .child_by_field_id(if implementation.kind_id() == self.trait_item {
                self.name
            } else {
                self.value_type
            })
            .and_then(|value_type| self.implementation_type(value_type))
        else {
            return Ok(());
        };
        let reference_implementation = value_type.kind_id() == self.reference_type;
        if element.kind_id() == self.parameter
            && !element
                .child_by_field_id(self.value_type)
                .is_some_and(|receiver| {
                    self.self_receiver_type(receiver, source, reference_implementation)
                })
        {
            return Ok(());
        }
        let mut cursor = element.walk();
        if let Some(name) = element
            .named_children(&mut cursor)
            .find(|child| child.kind_id() == self.self_value)
        {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::RustReceiver {
                    type_path: byte_range(value_type)?,
                },
            )?);
        }
        Ok(())
    }

    /// Whether a typed `self` parameter's type is a receiver of the implementation type:
    /// `Self`, a reference to a receiver, or one of the standard receiver wrappers `Box`,
    /// `Rc`, `Arc` and `Pin` around one. Boxed and pinned receivers of reference
    /// implementations need the wrapper itself to resolve their members.
    fn self_receiver_type(
        &self,
        mut element: Node<'_>,
        source: &str,
        reference_implementation: bool,
    ) -> bool {
        // Each step removes one wrapper from the bounded syntax tree.
        loop {
            if element.kind_id() == self.reference_type {
                let Some(inner) = element.child_by_field_id(self.value_type) else {
                    return false;
                };
                element = inner;
                continue;
            }
            if element.kind_id() == self.type_identifier {
                return element.utf8_text(source.as_bytes()) == Ok("Self");
            }
            if element.kind_id() != self.generic_type {
                return false;
            }
            let Some(name) = element
                .child_by_field_id(self.value_type)
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
            else {
                return false;
            };
            let name = name.rsplit("::").next().unwrap_or(name).trim();
            if !["Box", "Rc", "Arc", "Pin"].contains(&name)
                || (reference_implementation && matches!(name, "Box" | "Pin"))
            {
                return false;
            }
            let Some(arguments) = element.child_by_field_id(self.type_arguments) else {
                return false;
            };
            let mut cursor = arguments.walk();
            let mut types = arguments
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let (Some(inner), None) = (types.next(), types.next()) else {
                return false;
            };
            element = inner;
        }
    }

    fn record_bounds(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let parameter = element.kind_id() == self.type_parameter;
        let Some(name) = element
            .child_by_field_id(if parameter { self.name } else { self.left })
            .filter(|name| name.kind_id() == self.type_identifier)
        else {
            return Ok(());
        };
        if parameter
            && let Some(scope) = enclosing(
                visited,
                &[
                    self.functions[0],
                    self.functions[1],
                    self.impl_item,
                    self.trait_item,
                    self.type_item,
                ],
            )
            .map(Visited::syntax_element)
        {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::RustTypeParameter,
            )?);
        }
        if let Some(bounds) = element.child_by_field_id(self.bounds) {
            let mut cursor = bounds.walk();
            for bound in bounds.named_children(&mut cursor) {
                if self.non_method_bounds.contains(&bound.kind_id()) || bound.is_extra() {
                    continue;
                }
                let bound = self.named_type(bound).unwrap_or(bound);
                facts
                    .declared_types
                    .push((byte_range(name)?, byte_range(bound)?));
            }
        }
        Ok(())
    }

    fn record_impl(
        &self,
        syntax_element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if let Some(value_type) = syntax_element
            .child_by_field_id(self.value_type)
            .and_then(|value_type| self.implementation_type(value_type))
        {
            facts.inherent_impls.push(StaticInherentImpl {
                range: byte_range(syntax_element)?,
                type_path: byte_range(value_type)?,
            });
        }
        Ok(())
    }

    fn named_type<'tree>(&self, mut element: Node<'tree>) -> Option<Node<'tree>> {
        element = crate::rust::implemented_type(element);
        // Each step follows one grammar child, bounded by the captured syntax depth.
        while [self.generic_type, self.reference_type].contains(&element.kind_id()) {
            element = crate::rust::implemented_type(element.child_by_field_id(self.value_type)?);
        }
        (!element.is_missing()
            && [
                self.type_identifier,
                self.scoped_type_identifier,
                self.tuple_type,
                self.unit_type,
            ]
            .contains(&element.kind_id()))
        .then_some(element)
    }

    fn implementation_type<'tree>(&self, element: Node<'tree>) -> Option<Node<'tree>> {
        let element = crate::rust::implemented_type(element);
        if element.kind_id() == self.generic_type {
            return element.child_by_field_id(self.value_type);
        }
        ([
            self.type_identifier,
            self.scoped_type_identifier,
            self.reference_type,
            self.array_type,
            self.tuple_type,
            self.unit_type,
        ]
        .contains(&element.kind_id())
            && !element.is_missing())
        .then_some(element)
    }

    fn struct_value<'tree>(&self, syntax_element: Node<'tree>) -> Option<Node<'tree>> {
        let value = syntax_element.child_by_field_id(self.value)?;
        (value.kind_id() == self.struct_expression).then_some(())?;
        value.child_by_field_id(self.name)
    }

    fn record_closure_argument(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if let Some(scope) = enclosing(visited, &self.functions).map(Visited::syntax_element) {
            facts.bindings.push(binding(
                visited.syntax_element(),
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Shadow,
            )?);
        }
        Ok(())
    }

    fn record_return(
        &self,
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = element.child_by_field_id(self.name);
        let return_type = element
            .child_by_field_id(self.return_type)
            .and_then(|value| self.named_type(value));
        if let Some((name, return_type)) = name.zip(return_type) {
            facts
                .declared_types
                .push((byte_range(name)?, byte_range(return_type)?));
        }
        Ok(())
    }

    fn record_function_field(
        &self,
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(value) = element
            .child_by_field_id(self.value_type)
            .filter(|value| value.kind_id() == self.function_type)
        else {
            return Ok(());
        };
        if let Some(name) = element.child_by_field_id(self.name) {
            let result = value
                .child_by_field_id(self.return_type)
                .and_then(|result| self.named_type(result))
                .unwrap_or(value);
            facts
                .declared_types
                .push((byte_range(name)?, byte_range(result)?));
        }
        Ok(())
    }

    /// A named type item inside a block binds for the whole block.
    fn record_block_type(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let scope = enclosing(visited, &self.scopes)
            .map(Visited::syntax_element)
            .filter(|scope| scope.kind_id() == self.scopes[1]);
        let name = visited
            .syntax_element()
            .child_by_field_id(self.name)
            .filter(|name| {
                !name.is_missing()
                    && [self.identifier, self.type_identifier].contains(&name.kind_id())
            });
        if let Some((name, scope)) = name.zip(scope) {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::RustBlockType,
            )?);
        }
        Ok(())
    }

    /// Tuple and unit structs declare a value alongside their type.
    fn value_struct(&self, item: Node<'_>) -> bool {
        item.kind_id() == self.struct_item
            && item
                .child_by_field_id(self.body)
                .is_none_or(|body| body.kind_id() == self.tuple_fields)
    }

    /// Finds the block or module where a named value item binds its name.
    fn record_named_value_binding(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let item = visited.syntax_element();
        let container = visited.parent().and_then(Visited::parent);
        let owner = if item.kind_id() == self.functions[1] {
            container.filter(|parent| parent.syntax_element().kind_id() == self.foreign_item)
        } else {
            container
                .is_none_or(|parent| {
                    ![self.impl_item, self.trait_item].contains(&parent.syntax_element().kind_id())
                })
                .then_some(visited)
        };
        if let Some(owner) = owner {
            self.record_item_binding(item, owner, facts)?;
        }
        Ok(())
    }

    fn record_item_binding(
        &self,
        declaration: Node<'_>,
        item: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = declaration.child_by_field_id(self.name).filter(|name| {
            [self.identifier, self.type_identifier].contains(&name.kind_id()) && !name.is_missing()
        });
        let scope = enclosing(item, &self.scopes).map(Visited::syntax_element);
        if let Some((name, scope)) = name.zip(scope) {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Local,
            )?);
        }
        Ok(())
    }

    fn record_mutation(
        &self,
        syntax_element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if let Some(target) = syntax_element
            .child_by_field_id(self.left)
            .filter(|syntax_element| !syntax_element.is_missing())
        {
            facts.assignments.push(byte_range(target)?);
        }
        Ok(())
    }

    fn record_pattern(
        &self,
        element: Node<'_>,
        scope: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(pattern) = element.child_by_field_id(self.pattern) else {
            return Ok(());
        };
        let available_from =
            if [self.let_declaration, self.conditional[0]].contains(&element.kind_id()) {
                byte_range(element)?.end
            } else if element.kind_id() == self.conditional[2] {
                element
                    .child_by_field_id(self.body)
                    .map_or(byte_range(scope)?.start, |body| body.start_byte() as u64)
            } else {
                byte_range(scope)?.start
            };
        self.bind_pattern(pattern, scope, available_from, facts)
    }

    /// Binds every name a pattern introduces in `scope`: identifiers and `{ name }`
    /// shorthand fields, never the type path of a struct or tuple struct pattern. A macro
    /// in the pattern leaves the scope's bindings unsupported.
    fn bind_pattern(
        &self,
        pattern: Node<'_>,
        scope: Node<'_>,
        available_from: u64,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let mut pending = vec![pattern];
        let mut cursor = pattern.walk();
        while let Some(element) = pending.pop() {
            let kind = element.kind_id();
            if [
                self.scoped_identifier,
                self.scoped_type_identifier,
                self.type_identifier,
                self.attribute,
            ]
            .contains(&kind)
            {
                continue;
            }
            if kind == self.edge_macro {
                facts.unsupported_bindings.push(byte_range(scope)?);
                return Ok(());
            }
            if kind == self.identifier || kind == self.shorthand_field {
                facts.bindings.push(binding(
                    element,
                    scope,
                    available_from,
                    StaticBindingKind::Shadow,
                )?);
                continue;
            }
            // A match arm guard is an expression evaluated with the arm's bindings; it
            // introduces none of its own names.
            let path = if self.named_patterns.contains(&kind) {
                element.child_by_field_id(self.value_type)
            } else if kind == self.match_pattern {
                element.child_by_field_id(self.condition)
            } else {
                None
            };
            pending.extend(
                element
                    .named_children(&mut cursor)
                    .filter(|child| Some(*child) != path),
            );
        }
        Ok(())
    }

    fn record_patterns(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(scope) = enclosing(visited, &self.functions).map(Visited::syntax_element) else {
            return Ok(());
        };
        if syntax_element.kind_id() != self.closure_parameters {
            return Ok(());
        }
        // A closure's pattern parameters bind their names for the whole closure body.
        let mut cursor = syntax_element.walk();
        let patterns = syntax_element
            .named_children(&mut cursor)
            .filter(|child| {
                !child.is_extra()
                    && child.kind_id() != self.parameter
                    && child.kind_id() != self.identifier
            })
            .collect::<Vec<_>>();
        for pattern in patterns {
            if pattern.is_missing() {
                continue;
            }
            self.bind_pattern(pattern, scope, byte_range(scope)?.start, facts)?;
        }
        Ok(())
    }
}

/// Whether `text` closes every delimiter it opens and none it does not.
fn closed(text: &str) -> bool {
    let mut open = Vec::new();
    for character in text.chars() {
        match character {
            '(' => open.push(')'),
            '[' => open.push(']'),
            '{' => open.push('}'),
            ')' | ']' | '}' if open.pop() != Some(character) => return false,
            _ => {}
        }
    }
    open.is_empty()
}

/// The identifiers written in `text`.
fn identifiers(text: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let part = |character: char| character.is_alphanumeric() || character == '_';
    text.split(move |character| !part(character))
        .filter(|word| {
            word.chars()
                .next()
                .is_some_and(|first| first.is_alphabetic() || first == '_')
        })
        .map(move |word| {
            let start = word.as_ptr() as usize - text.as_ptr() as usize;
            start..start + word.len()
        })
}
