use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;
use tree_sitter::Node;

use crate::{ByteRange, StaticBinding, StaticBindingKind, StaticFacts, SyntaxSymbol};

use super::{
    BLOCK_KIND, CLASS_KIND, DECORATED_KIND, DECORATOR_KIND, FUNCTION_KIND, METHOD_KIND_WORD,
};

/// Repeated receiver names can share a declaration identity when the methods
/// form one checked property slot.
pub(super) fn receivers(
    root: Node<'_>,
    source: &str,
    symbols: &[SyntaxSymbol],
    facts: &StaticFacts,
    counts: &BTreeMap<&str, usize>,
) -> BTreeSet<ByteRange> {
    if root.has_error()
        || !symbols.iter().any(|symbol| {
            symbol.is_python_parameter() && counts.get(symbol.qualified_name.as_str()) != Some(&1)
        })
    {
        return BTreeSet::new();
    }
    let bindings: BTreeMap<_, _> = facts.bindings().iter().map(|b| (b.name, b)).collect();
    let mut parameters = BTreeMap::<_, Vec<_>>::new();
    for symbol in symbols.iter().filter(|symbol| symbol.is_python_parameter()) {
        if counts.get(symbol.qualified_name.as_str()) == Some(&1) {
            continue;
        }
        if let Some((owner, binding)) = symbol
            .container
            .as_deref()
            .zip(symbol.name_range.and_then(|range| bindings.get(&range)))
            && matches!(binding.kind, StaticBindingKind::PythonReceiver { .. })
        {
            parameters
                .entry(owner)
                .or_default()
                .push((symbol, *binding));
        }
    }
    if parameters.is_empty() {
        return BTreeSet::new();
    }
    let mut families = BTreeMap::<_, Vec<_>>::new();
    for symbol in symbols
        .iter()
        .filter(|symbol| symbol.kind == METHOD_KIND_WORD)
    {
        let owner = symbol.qualified_name.as_str();
        if parameters.contains_key(owner) {
            families.entry(owner).or_default().push(symbol);
        }
    }
    let names = families
        .values()
        .filter_map(|family| family.first().map(|symbol| symbol.name.as_str()))
        .collect::<BTreeSet<_>>();
    let slots = SlotBindings::new(root, source, facts, &names);
    let mut retained = BTreeSet::new();
    for (owner, mut family) in families {
        if !(2..=3).contains(&family.len()) || counts.get(owner) != Some(&family.len()) {
            continue;
        }
        family.sort_by_key(|symbol| symbol.item_range);
        let Some(binding) = family[0].name_range.and_then(|range| bindings.get(&range)) else {
            continue;
        };
        let family_name = family[0].name.as_str();
        let scope = binding.scope;
        if !slots.admits(scope, family_name, family.len()) {
            continue;
        }
        let mut accessors = BTreeSet::new();
        if !family.iter().enumerate().all(|(index, method)| {
            let Some(binding) = method.name_range.and_then(|name| bindings.get(&name)) else {
                return false;
            };
            if binding.scope != scope {
                return false;
            }
            match decorator(root, source, binding) {
                Some(Decorator::Property) => index == 0,
                Some(Decorator::Accessor { name, facet }) => {
                    index > 0 && name == family_name && accessors.insert(facet)
                }
                None => false,
            }
        }) {
            continue;
        }
        let mut repeated = BTreeMap::<_, BTreeMap<_, _>>::new();
        for &(parameter, binding) in &parameters[owner] {
            if family
                .iter()
                .any(|method| method.item_range == binding.scope)
            {
                repeated
                    .entry(parameter.qualified_name.as_str())
                    .or_default()
                    .insert(binding.scope, binding.name);
            }
        }
        for (name, receivers) in repeated {
            if receivers.len() > 1 && counts.get(name) == Some(&receivers.len()) {
                retained.extend(receivers.into_values());
            }
        }
    }
    retained
}

/// Checks for a competing binding of the class slot or the property decorator.
struct SlotBindings<'source> {
    counts: BTreeMap<(ByteRange, &'source str), usize>,
    blocked: Vec<ByteRange>,
}

impl<'source> SlotBindings<'source> {
    fn new(
        root: Node<'_>,
        source: &'source str,
        facts: &StaticFacts,
        names: &BTreeSet<&str>,
    ) -> Self {
        let mut scoped_counts = BTreeMap::<_, usize>::new();
        let mut blocked = facts.unsupported_bindings().to_vec();
        for binding in facts.bindings() {
            if let Some(name) = source_text(source, binding.name) {
                *scoped_counts.entry((binding.scope, name)).or_default() += 1;
                if name == "property" {
                    blocked.push(binding.scope);
                }
            }
        }
        for reference in facts.references() {
            if reference.role == ReferenceRole::Write
                && let Some(name) = source_text(source, reference.target)
                && (name == "property" || names.contains(name))
                && let Some(scope) = written_scope(root, reference.target)
            {
                *scoped_counts.entry((scope, name)).or_default() += 1;
                if name == "property" {
                    blocked.push(scope);
                }
            }
        }
        blocked.sort_unstable();
        let mut end = 0;
        for scope in &mut blocked {
            end = end.max(scope.end);
            scope.end = end;
        }
        Self {
            counts: scoped_counts,
            blocked,
        }
    }

    fn admits(&self, scope: ByteRange, name: &str, count: usize) -> bool {
        let before = self
            .blocked
            .partition_point(|blocker| blocker.start <= scope.start);
        self.counts.get(&(scope, name)) == Some(&count)
            && (before == 0 || !self.blocked[before - 1].contains(scope.start))
    }
}

enum Decorator<'source> {
    Property,
    Accessor {
        name: &'source str,
        facet: &'source str,
    },
}

fn decorator<'source>(
    root: Node<'_>,
    source: &'source str,
    method: &StaticBinding,
) -> Option<Decorator<'source>> {
    let name = at_range(root, method.name)?;
    let function = name.parent().filter(|node| {
        node.kind() == FUNCTION_KIND && node.child_by_field_name("name") == Some(name)
    })?;
    let decorated = function
        .parent()
        .filter(|node| node.kind() == DECORATED_KIND)?;
    let class = decorated
        .parent()
        .filter(|node| node.kind() == BLOCK_KIND)?
        .parent()?;
    if class.kind() != CLASS_KIND {
        return None;
    }
    let mut cursor = decorated.walk();
    let mut decorators = decorated
        .named_children(&mut cursor)
        .filter(|node| node.kind() == DECORATOR_KIND);
    let expression = decorators.next()?.named_child(0)?;
    if decorators.next().is_some() {
        return None;
    }
    match expression.kind() {
        "identifier" if source.get(expression.byte_range())? == "property" => {
            Some(Decorator::Property)
        }
        "attribute" => {
            let object = expression
                .child_by_field_name("object")
                .filter(|node| node.kind() == "identifier")?;
            let facet = source.get(expression.child_by_field_name("attribute")?.byte_range())?;
            matches!(facet, "setter" | "deleter").then_some(Decorator::Accessor {
                name: source.get(object.byte_range())?,
                facet,
            })
        }
        _ => None,
    }
}

fn written_scope(root: Node<'_>, range: ByteRange) -> Option<ByteRange> {
    let mut current = at_range(root, range)?;
    loop {
        match current.kind() {
            "module" => {
                let range = crate::extract::byte_range(current).ok()?;
                return Some(ByteRange { start: 0, ..range });
            }
            CLASS_KIND | FUNCTION_KIND => return crate::extract::byte_range(current).ok(),
            _ => current = current.parent()?,
        }
    }
}

fn at_range(root: Node<'_>, range: ByteRange) -> Option<Node<'_>> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    root.named_descendant_for_byte_range(start, end)
}

fn source_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
