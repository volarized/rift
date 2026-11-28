//! Physical local owners of literal properties written by a direct assignment.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::{identity::SymbolIdentity, read::SymbolKind};
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticAssignedObject, StaticBinding, StaticBindingKind,
    StaticFacts,
};

use super::{SelectedFile, binding_name};
use crate::EnclosingDefinitions;

/// Places source-complete properties below the checked local their literal writes.
pub(super) fn place(
    file: &SelectedFile<'_>,
    anchors: &BTreeMap<String, SymbolId>,
    scoped: &mut ScopedIdentityAnchors,
) {
    if file.syntax.language().name != "javascript"
        || file.syntax.has_errors()
        || file.syntax.left_out_declaration_count() != 0
    {
        return;
    }
    let Some(facts) = file.syntax.static_facts() else {
        return;
    };
    let owners = assigned_literal_owners(file, facts, anchors, scoped);
    if owners.is_empty() {
        return;
    }
    let symbols = file.syntax.symbols();
    let holders = facts
        .body_members()
        .iter()
        .map(|member| (member.member, member))
        .collect::<BTreeMap<_, _>>();
    let mut occurrences = BTreeMap::new();
    for symbol in symbols {
        if let Some(member) = holders.get(&symbol.item_range)
            && owners.contains_key(&member.body)
            && let Some(name) = symbol
                .name_range
                .and_then(|name| binding_name(file.source, name))
        {
            *occurrences.entry((member.body, name)).or_insert(0_usize) += 1;
        }
    }
    let mut occupied = anchors
        .values()
        .chain(scoped.values().flat_map(BTreeMap::values))
        .cloned()
        .collect::<BTreeSet<_>>();
    for property in symbols {
        let Some(member) = holders.get(&property.item_range) else {
            continue;
        };
        let Some(owner) = owners.get(&member.body) else {
            continue;
        };
        let name = property.qualified_name.as_str();
        let Some(key) = property
            .name_range
            .and_then(|key| binding_name(file.source, key))
        else {
            continue;
        };
        if property.container.is_some()
            || property.kind != "property"
            || property.kinds.contains(&SymbolKind::Public)
            || !matches!(
                property.syntax_element_kind,
                Some("pair" | "shorthand_property_identifier")
            )
            || property.name.starts_with('[')
            || name != property.name
            || (key != property.name
                && property
                    .name_range
                    .and_then(|range| text(file.source, range))
                    != Some(name))
            || occurrences.get(&(member.body, key)) != Some(&1)
            || !anchors.contains_key(name)
            || scoped
                .get(name)
                .is_some_and(|group| group.contains_key(&property.declaration_ordinal))
        {
            continue;
        }
        let mut path = owner.qualified_path().to_vec();
        path.push(property.name.clone());
        let Ok(identity) =
            SymbolIdentity::new(owner.owner().clone(), owner.language().clone(), path)
        else {
            continue;
        };
        let Ok(id) = SymbolId::new(identity.wire_identity()) else {
            continue;
        };
        if occupied.insert(id.clone()) {
            scoped
                .entry(name.to_owned())
                .or_default()
                .insert(property.declaration_ordinal, id);
        }
    }
}

fn assigned_literal_owners(
    file: &SelectedFile<'_>,
    facts: &StaticFacts,
    anchors: &BTreeMap<String, SymbolId>,
    scoped: &ScopedIdentityAnchors,
) -> BTreeMap<ByteRange, SymbolIdentity> {
    if facts.assigned_objects().is_empty() {
        return BTreeMap::new();
    }
    let source = file.source;
    let callees = facts
        .calls()
        .iter()
        .map(|call| call.callee_range)
        .collect::<BTreeSet<_>>();
    let writes = facts
        .function_writes()
        .iter()
        .map(|target| target.end)
        .collect::<BTreeSet<_>>();
    let functions = facts
        .function_values()
        .iter()
        .filter_map(|&(function, value)| {
            (callees.contains(&value) || written_assigned_named_function(source, function, &writes))
                .then_some(function)
        })
        .collect::<BTreeSet<_>>();
    if functions.is_empty() {
        return BTreeMap::new();
    }
    let physical = physical_function_ranges(file, facts);
    let mut named = BTreeMap::<&str, Vec<&StaticBinding>>::new();
    for binding in facts.bindings() {
        if let Some(name) = text(source, binding.name) {
            named.entry(name).or_default().push(binding);
        }
    }
    let declarations = file
        .syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            (symbol.syntax_element_kind == Some("variable_declarator")
                && symbol.kind == "variable"
                && symbol.body_range.is_none()
                && !symbol.kinds.contains(&SymbolKind::Public))
            .then_some((symbol.name_range?, symbol))
        })
        .collect::<BTreeMap<_, _>>();
    facts
        .assigned_objects()
        .iter()
        .filter_map(|write| {
            let name = text(source, write.target)?;
            let binding = checked_binding(write, named.get(name)?)?;
            let function = physical.resolve(write.target.start, write.object.end)?;
            if *function != binding.scope || !functions.contains(function) {
                return None;
            }
            let declared = declarations.get(&binding.name)?;
            let owner = scoped
                .get(&declared.qualified_name)
                .and_then(|rows| rows.get(&declared.declaration_ordinal))
                .or_else(|| anchors.get(&declared.qualified_name))?;
            Some((write.object, SymbolIdentity::parse(owner.as_str()).ok()?))
        })
        .collect()
}

/// Requires the direct written name of the exact assignment's function value.
fn written_assigned_named_function(
    source: &str,
    function: ByteRange,
    writes: &BTreeSet<u64>,
) -> bool {
    let checked = || {
        let body = text(source, function)?.strip_prefix("function")?;
        if !body.starts_with(char::is_whitespace) {
            return None;
        }
        let (name, _) = body.split_once('(')?;
        let name = name.trim();
        let target_end = *writes.range(..function.start).next_back()?;
        let operator = text(
            source,
            ByteRange {
                start: target_end,
                end: function.start,
            },
        )?;
        let mut characters = name.bytes();
        let initial = characters.next()?;
        ((initial.is_ascii_alphabetic() || matches!(initial, b'_' | b'$'))
            && characters.all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, b'_' | b'$')
            })
            && operator.trim() == "=")
            .then_some(())
    };
    checked().is_some()
}

fn physical_function_ranges(
    file: &SelectedFile<'_>,
    facts: &StaticFacts,
) -> EnclosingDefinitions<ByteRange> {
    let spans = facts
        .function_values()
        .iter()
        .map(|&(function, _value)| function)
        .chain(
            file.syntax
                .symbols()
                .iter()
                .filter(|symbol| symbol.kinds.contains(&SymbolKind::Callable))
                .map(|symbol| symbol.item_range),
        )
        .collect::<BTreeSet<_>>();
    EnclosingDefinitions::new(spans.into_iter().map(|span| (span.start, span.end, span)))
}

fn checked_binding<'a>(
    write: &StaticAssignedObject,
    bindings: &[&'a StaticBinding],
) -> Option<&'a StaticBinding> {
    let mut visible = bindings.iter().copied().filter(|binding| {
        binding.scope.encloses(write.block) && binding.scope.encloses(write.target)
    });
    let governing = visible
        .clone()
        .min_by_key(|binding| binding.scope.end - binding.scope.start)?;
    (governing.kind == StaticBindingKind::Local
        && governing.available_from <= write.target.start
        && governing.name.end <= write.target.start
        && !visible.any(|other| other.scope == governing.scope && other.name != governing.name))
    .then_some(governing)
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
