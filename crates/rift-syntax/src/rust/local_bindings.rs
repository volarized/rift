//! Simple initialized names and parameters that an authored Rust function binds.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;
use tree_sitter::Node;

use crate::extract::Visited;
use crate::static_analysis::{StaticBindingKind, StaticFacts, StaticReference};
use crate::{ByteRange, SyntaxElement, SyntaxSymbol};

pub(super) const KIND: &str = "local_binding";
pub(super) const PARAMETER_KIND: &str = "parameter_binding";

/// The physical identifier of a supported let statement or value parameter.
pub(super) fn pattern(item: Node<'_>) -> Option<Node<'_>> {
    if item.has_error() {
        return None;
    }
    let pattern = item.child_by_field_name("pattern")?;
    let eligible = match item.kind() {
        "parameter" => true,
        "let_declaration" => item.child_by_field_name("value")?.kind() != "closure_expression",
        _ => false,
    };
    (eligible && pattern.kind() == "identifier").then_some(pattern)
}

/// A plain authored parameter of a named function with a body.
pub(super) fn parameter_name<'source>(
    visited: Visited<'_, '_>,
    source: &'source str,
) -> Option<&'source str> {
    let parameters = visited.parent()?;
    if parameters.syntax_element().kind() != "parameters"
        || parameters.parent()?.syntax_element().kind() != "function_item"
    {
        return None;
    }
    let name = source.get(pattern(visited.syntax_element())?.byte_range())?;
    plain_name(name).then_some(name)
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
        matches!(self.kind, KIND | PARAMETER_KIND)
    }

    /// Whether this source-local Rust binding is a parameter.
    #[must_use]
    pub fn is_rust_parameter_binding(&self) -> bool {
        self.kind == PARAMETER_KIND
    }

    /// The generated last path component of a physical Rust lexical binding.
    #[must_use]
    pub fn rust_local_binding_component(&self) -> Option<&str> {
        if !self.is_rust_local_binding() {
            return None;
        }
        let (owner, component) = self.qualified_name.rsplit_once("::")?;
        let (name, ordinal) = component.rsplit_once('#')?;
        let ordinal = if self.is_rust_parameter_binding() {
            ordinal.strip_prefix("parameter")?
        } else {
            ordinal
        };
        (self.container.as_deref() == Some(owner)
            && name == self.name
            && plain_name(name)
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
    if facts.rust_uses().iter().any(|import| import.glob)
        || !facts.unsupported_bindings().is_empty()
        || !facts.rust_root_macro_calls().is_empty()
        || !facts.rust_textual_macro_calls().is_empty()
        || !facts.rust_macro_barriers().is_empty()
    {
        symbols.retain(|symbol| !symbol.is_rust_local_binding());
        return;
    }
    let opaque = opaque_names(source, symbols, facts);
    let mut same_name = BTreeMap::new();
    let mut retained = BTreeSet::new();
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
        if symbol.item_range.end <= conditional_end {
            return false;
        }
        let Some(name) = source_name(source, symbol.name_range) else {
            return false;
        };
        let Some(owner) = symbol.container.as_deref() else {
            return false;
        };
        if opaque.contains(name) {
            return false;
        }
        let ordinal = same_name
            .entry((owner.to_owned(), name, symbol.kind))
            .or_insert(0_usize);
        // A Rust identifier cannot contain this ordinal separator.
        let category = if symbol.kind == PARAMETER_KIND {
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
        retained.insert(name);
        true
    });
    if retained.is_empty() {
        return;
    }
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
    let reads = direct
        .chain(binary)
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
