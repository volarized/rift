//! Simple initialized names and parameters that an authored Rust function binds.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;
use tree_sitter::Node;

use crate::extract::Visited;
use crate::static_analysis::{StaticBindingKind, StaticFacts, StaticReference};
use crate::{ByteRange, SyntaxElement, SyntaxSymbol};

pub(super) const KIND: &str = "local_binding";
pub(super) const PARAMETER_KIND: &str = "parameter_binding";
pub(super) const EXPLICIT_LOCAL_KIND: &str = "explicit_local_binding";
pub(super) const SHORTHAND_KIND: &str = "shorthand_binding";

/// The physical identifier of a supported let statement or value parameter.
pub(super) fn pattern(item: Node<'_>) -> Option<Node<'_>> {
    if item.has_error() {
        return None;
    }
    if item.kind() == "shorthand_field_identifier" {
        return Some(item);
    }
    if generic_kind(item.kind()) {
        return item.child_by_field_name("name");
    }
    if let Some(receiver) = receiver(item) {
        return Some(receiver);
    }
    let pattern = item.child_by_field_name("pattern")?;
    let eligible = match item.kind() {
        "parameter" => true,
        "let_declaration" => item.child_by_field_name("value")?.kind() != "closure_expression",
        _ => false,
    };
    (eligible && pattern.kind() == "identifier").then_some(pattern)
}

/// The written `self` token, including the token in a typed receiver.
pub(super) fn receiver(item: Node<'_>) -> Option<Node<'_>> {
    match item.kind() {
        "self_parameter" => item
            .named_children(&mut item.walk())
            .find(|child| child.kind() == "self"),
        "parameter" => item
            .child_by_field_name("pattern")
            .filter(|pattern| pattern.kind() == "self"),
        _ => None,
    }
}

fn generic_kind(kind: &str) -> bool {
    matches!(
        kind,
        "type_parameter" | "lifetime_parameter" | "const_parameter"
    )
}

/// The physical name in the generic parameter list of an authored item.
pub(super) fn generic_name<'source>(
    visited: Visited<'_, '_>,
    source: &'source str,
) -> Option<&'source str> {
    let parameters = visited.parent()?;
    let owner = parameters.parent()?.syntax_element();
    if parameters.syntax_element().kind() != "type_parameters"
        || !matches!(
            owner.kind(),
            "function_item"
                | "function_signature_item"
                | "impl_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "trait_item"
                | "type_item"
                | "associated_type"
        )
    {
        return None;
    }
    let name = source.get(pattern(visited.syntax_element())?.byte_range())?;
    rift_core::is_portable_name(name).then_some(name)
}

/// A shorthand field in a checked binding position of a named function.
pub(super) fn shorthand_name<'source>(
    visited: Visited<'_, '_>,
    source: &'source str,
) -> Option<&'source str> {
    let item = visited.syntax_element();
    let name = source.get(pattern(item)?.byte_range())?;
    if !plain_name(name) || visited.parent()?.syntax_element().kind() != "field_pattern" {
        return None;
    }
    let mut ancestor = visited.parent().and_then(Visited::parent);
    let mut binding = false;
    while let Some(outer) = ancestor {
        match outer.syntax_element().kind() {
            "let_declaration" | "parameter" | "match_arm" => binding = true,
            "function_item" => return binding.then_some(name),
            "or_pattern" | "macro_invocation" | "closure_expression" | "async_block"
            | "const_block" | "const_item" | "static_item" => return None,
            _ => {}
        }
        ancestor = outer.parent();
    }
    None
}

/// A plain authored function parameter or a method's receiver.
pub(super) fn parameter_name<'source>(
    visited: Visited<'_, '_>,
    source: &'source str,
) -> Option<&'source str> {
    let parameters = visited.parent()?;
    let function = parameters.parent()?;
    if parameters.syntax_element().kind() != "parameters" {
        return None;
    }
    let item = visited.syntax_element();
    let name = source.get(pattern(item)?.byte_range())?;
    if receiver(item).is_some() {
        let list = function.parent()?;
        return (matches!(
            function.syntax_element().kind(),
            "function_item" | "function_signature_item"
        ) && list.syntax_element().kind() == "declaration_list"
            && matches!(
                list.parent()?.syntax_element().kind(),
                "impl_item" | "trait_item"
            ))
        .then_some(name);
    }
    if function.syntax_element().kind() != "function_item" {
        return None;
    }
    plain_name(name).then_some(name)
}

/// Whether this let writes a mutable binding rather than a named pattern path.
pub(super) fn explicit_mut(item: Node<'_>) -> bool {
    item.kind() == "let_declaration"
        && item
            .named_children(&mut item.walk())
            .any(|child| child.kind() == "mutable_specifier")
}

/// An initialized, plain binding in a named function, outside other bodies.
pub(super) fn name<'source>(
    visited: Visited<'_, '_>,
    source: &'source str,
) -> Option<&'source str> {
    let pattern = pattern(visited.syntax_element())?;
    if visited.parent()?.syntax_element().kind() != "block" {
        return None;
    }
    let name = source.get(pattern.byte_range())?;
    if !plain_name(name) {
        return None;
    }
    let mut ancestor = visited.parent();
    while let Some(outer) = ancestor {
        match outer.syntax_element().kind() {
            "function_item" => return Some(name),
            "async_block" | "closure_expression" | "const_block" | "const_item" | "static_item" => {
                return None;
            }
            _ => ancestor = outer.parent(),
        }
    }
    None
}

/// The presently collected spelling class has no raw identifiers or prelude variant names.
pub(crate) fn plain_name(name: &str) -> bool {
    name != "_"
        && name
            .as_bytes()
            .first()
            .is_some_and(|first| first.is_ascii_lowercase() || *first == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl SyntaxSymbol {
    /// Whether this is a source-local Rust binding candidate.
    #[must_use]
    pub fn is_rust_local_binding(&self) -> bool {
        matches!(
            self.kind,
            KIND | PARAMETER_KIND | EXPLICIT_LOCAL_KIND | SHORTHAND_KIND
        ) || self.is_rust_generic_parameter()
    }

    /// Whether this source-local Rust binding is a parameter.
    #[must_use]
    pub fn is_rust_parameter_binding(&self) -> bool {
        self.kind == PARAMETER_KIND
    }

    /// Whether this is a method's reserved receiver parameter.
    #[must_use]
    pub fn is_rust_receiver_binding(&self) -> bool {
        self.is_rust_parameter_binding() && self.name == "self"
    }

    /// Whether this is a source-local Rust type, lifetime or const parameter.
    #[must_use]
    pub fn is_rust_generic_parameter(&self) -> bool {
        generic_kind(self.kind)
    }

    /// Whether the written modifier establishes a declaration in an opaque scope.
    #[must_use]
    pub fn is_rust_explicit_local_binding(&self) -> bool {
        self.kind == EXPLICIT_LOCAL_KIND
    }

    /// Whether this declaration is a shorthand struct field binding.
    #[must_use]
    pub fn is_rust_shorthand_binding(&self) -> bool {
        self.kind == SHORTHAND_KIND
    }

    /// The generated last path component of a physical Rust lexical binding.
    #[must_use]
    pub fn rust_local_binding_component(&self) -> Option<&str> {
        if !self.is_rust_local_binding() {
            return None;
        }
        let (owner, component) = self.qualified_name.rsplit_once("::")?;
        let (name, ordinal) = component.rsplit_once('#')?;
        let ordinal = if self.is_rust_explicit_local_binding() {
            ordinal.strip_prefix("explicit")?
        } else if self.is_rust_shorthand_binding() {
            ordinal.strip_prefix("shorthand")?
        } else if self.is_rust_generic_parameter() {
            ordinal.strip_prefix("generic")?
        } else if self.is_rust_parameter_binding() {
            ordinal.strip_prefix("parameter")?
        } else {
            ordinal
        };
        (self.container.as_deref() == Some(owner)
            && name == self.name
            && (self.is_rust_generic_parameter() || plain_name(name))
            && ordinal
                .parse::<usize>()
                .is_ok_and(|value| value.to_string() == ordinal))
        .then_some(component)
    }
}

/// A syntactic name can instead be a path pattern if scope makes an item visible.
pub(super) fn prepare(
    source: &str,
    elements: &[SyntaxElement],
    symbols: &mut Vec<SyntaxSymbol>,
    facts: &mut StaticFacts,
) {
    if !symbols.iter().any(SyntaxSymbol::is_rust_local_binding) {
        return;
    }
    let opaque_scope = facts.rust_uses().iter().any(|import| import.glob)
        || !facts.unsupported_bindings().is_empty()
        || !facts.rust_root_macro_calls().is_empty()
        || !facts.rust_textual_macro_calls().is_empty()
        || !facts.rust_macro_barriers().is_empty();
    if opaque_scope {
        symbols.retain(|symbol| {
            !symbol.is_rust_local_binding()
                || symbol.is_rust_receiver_binding()
                || symbol.is_rust_generic_parameter()
                || symbol.is_rust_explicit_local_binding()
        });
        if !symbols.iter().any(SyntaxSymbol::is_rust_local_binding) {
            return;
        }
    } else {
        for symbol in symbols
            .iter_mut()
            .filter(|symbol| symbol.is_rust_explicit_local_binding())
        {
            symbol.kind = KIND;
        }
    }
    prepare_bindings(source, elements, symbols, facts, opaque_scope);
}

fn prepare_bindings(
    source: &str,
    elements: &[SyntaxElement],
    symbols: &mut Vec<SyntaxSymbol>,
    facts: &mut StaticFacts,
    opaque_scope: bool,
) {
    let opaque = if opaque_scope {
        BTreeSet::new()
    } else {
        opaque_names(source, symbols, facts)
    };
    let shorthand_sites = if symbols.iter().any(SyntaxSymbol::is_rust_shorthand_binding) {
        facts
            .bindings()
            .iter()
            .filter(|binding| binding.kind == StaticBindingKind::Shadow)
            .map(|binding| binding.name)
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    let mut same_name = BTreeMap::new();
    let mut retained = BTreeSet::new();
    let mut retained_shorthand = BTreeSet::new();
    let mut has_receivers = false;
    let mut conditions = facts
        .conditional_items()
        .iter()
        .map(|item| item.range)
        .collect::<Vec<_>>();
    conditions.sort_unstable();
    let mut conditions = conditions.into_iter().peekable();
    let mut conditional_end = 0;
    symbols.retain_mut(|symbol| {
        if !symbol.is_rust_local_binding() {
            return true;
        }
        while let Some(condition) =
            conditions.next_if(|condition| condition.start <= symbol.item_range.start)
        {
            conditional_end = conditional_end.max(condition.end);
        }
        let receiver = symbol.is_rust_receiver_binding();
        let generic = symbol.is_rust_generic_parameter();
        let explicit = symbol.is_rust_explicit_local_binding();
        let shorthand = symbol.is_rust_shorthand_binding();
        if shorthand
            && symbol
                .name_range
                .is_none_or(|range| !shorthand_sites.contains(&range))
        {
            return false;
        }
        if !receiver && !generic && symbol.item_range.end <= conditional_end {
            return false;
        }
        let Some(name) = source_name(source, symbol.name_range) else {
            return false;
        };
        let Some(owner) = symbol.container.as_deref() else {
            return false;
        };
        if !receiver && !generic && !explicit && opaque.contains(name) {
            return false;
        }
        let ordinal = same_name
            .entry((owner.to_owned(), name, symbol.kind))
            .or_insert(0_usize);
        // A Rust identifier cannot contain this ordinal separator.
        let category = if explicit {
            "explicit"
        } else if shorthand {
            "shorthand"
        } else if generic {
            "generic"
        } else if symbol.kind == PARAMETER_KIND {
            "parameter"
        } else {
            ""
        };
        let qualified = format!("{owner}::{name}#{category}{ordinal}");
        if !rift_core::is_portable_name(name) || !rift_core::is_portable_name(&qualified) {
            return false;
        }
        symbol.qualified_name = qualified;
        *ordinal += 1;
        if receiver {
            has_receivers = true;
        } else if !generic && !explicit {
            retained.insert(name);
            if shorthand {
                retained_shorthand.insert(name);
            }
        }
        true
    });
    if !retained.is_empty() {
        prepare_direct_reads(source, elements, facts, &retained, &retained_shorthand);
    }
    if has_receivers {
        let reads = elements
            .iter()
            .filter_map(|element| receiver_reference(element, elements));
        facts.add_rust_local_references(reads.collect());
    }
}

fn prepare_direct_reads(
    source: &str,
    elements: &[SyntaxElement],
    facts: &mut StaticFacts,
    retained: &BTreeSet<&str>,
    shorthand: &BTreeSet<&str>,
) {
    let direct = facts
        .rust_value_reads()
        .iter()
        .filter(|read| !read.pattern)
        .map(|read| read.range);
    let binary = elements.iter().filter_map(|element| {
        let parent = elements.get(element.parent?)?;
        (element.kind == "identifier" && parent.kind == "binary_expression")
            .then_some(element.range)
    });
    let match_values = elements.iter().filter_map(|element| {
        let parent = elements.get(element.parent?)?;
        (element.kind == "identifier"
            && parent.kind == "match_arm"
            && source_name(source, Some(element.range))
                .is_some_and(|name| shorthand.contains(name)))
        .then_some(element.range)
    });
    let reads = direct
        .chain(binary)
        .chain(match_values)
        .filter(|&range| {
            source_name(source, Some(range)).is_some_and(|name| retained.contains(name))
        })
        .map(|target| StaticReference {
            target,
            role: ReferenceRole::Read,
        })
        .collect();
    facts.add_rust_local_references(reads);
}

fn receiver_reference(
    element: &SyntaxElement,
    elements: &[SyntaxElement],
) -> Option<StaticReference> {
    if element.kind != "self" {
        return None;
    }
    let parent = elements.get(element.parent?)?;
    // The same token is also a qualifier in paths and imports. Its immediate expression
    // parent identifies written receiver uses.
    matches!(
        parent.kind,
        "field_expression"
            | "arguments"
            | "binary_expression"
            | "unary_expression"
            | "reference_expression"
            | "parenthesized_expression"
            | "cast_expression"
            | "return_expression"
            | "expression_statement"
            | "let_declaration"
            | "block"
            | "tuple_expression"
            | "array_expression"
            | "index_expression"
            | "try_expression"
            | "await_expression"
            | "field_initializer"
    )
    .then_some(StaticReference {
        target: element.range,
        role: ReferenceRole::Read,
    })
}

fn opaque_names<'source>(
    source: &'source str,
    symbols: &[SyntaxSymbol],
    facts: &StaticFacts,
) -> BTreeSet<&'source str> {
    let mut opaque = BTreeSet::new();
    for symbol in symbols
        .iter()
        .filter(|symbol| matches!(symbol.kind, "constant" | "static" | "struct" | "variant"))
    {
        if let Some(name) = source_name(source, symbol.name_range) {
            opaque.insert(name.strip_prefix("r#").unwrap_or(name));
        }
    }
    for binding in facts.bindings() {
        if let Some(name) = source_name(source, Some(binding.name)) {
            if let Some(raw) = name.strip_prefix("r#") {
                opaque.insert(raw);
            } else if matches!(binding.kind, StaticBindingKind::RustUse { .. }) {
                opaque.insert(name);
            }
        }
    }
    opaque
}

fn source_name(source: &str, range: Option<ByteRange>) -> Option<&str> {
    let range = range?;
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
