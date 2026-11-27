//! Places literal bodies written in assignments to checked local object members.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_syntax::{
    ByteRange, StaticBinding, StaticBindingKind, StaticFacts, StaticMember, SyntaxSymbol,
};

use super::{SelectedFile, ordinary_private_property, written_array};
use crate::EnclosingDefinitions;

pub(super) fn array_owners<'source>(
    file: &SelectedFile<'source>,
    facts: &'source StaticFacts,
    anchors: &BTreeMap<String, SymbolId>,
    declarations: &BTreeMap<&'source str, Option<&'source SyntaxSymbol>>,
    callables: &EnclosingDefinitions<(ByteRange, Option<&'source SyntaxSymbol>)>,
) -> BTreeMap<ByteRange, &'source str> {
    let source = file.source;
    let assigned = facts.assignments().iter().copied().collect::<BTreeSet<_>>();
    let targets = facts
        .members()
        .iter()
        .filter(|member| assigned.contains(&member.range) && direct_member(source, member))
        .map(|member| (member.range.end, member))
        .collect::<BTreeMap<_, _>>();
    if targets.is_empty() {
        return BTreeMap::new();
    }
    let mut bindings = BTreeMap::<&str, Vec<&StaticBinding>>::new();
    for binding in facts.bindings() {
        if let Some(name) = text(source, binding.name) {
            bindings.entry(name).or_default().push(binding);
        }
    }
    let mut writes = BTreeMap::<&str, Vec<ByteRange>>::new();
    for range in &assigned {
        if let Some(name) = text(source, *range).filter(|name| bindings.contains_key(name)) {
            writes.entry(name).or_default().push(*range);
        }
    }
    let bodies = facts
        .body_members()
        .iter()
        .map(|member| (member.member, member.body))
        .collect::<BTreeMap<_, _>>();
    let holders = facts
        .body_members()
        .iter()
        .map(|member| member.holder)
        .filter(|holder| written_array(source, *holder))
        .collect::<BTreeSet<_>>();
    holders
        .into_iter()
        .filter_map(|holder| {
            let (_, target) = targets.range(..=holder.start).next_back()?;
            let operator = text(
                source,
                ByteRange {
                    start: target.range.end,
                    end: holder.start,
                },
            )?;
            if operator.trim() != "=" {
                return None;
            }
            let (_, owner) = callables.resolve(target.range.start, holder.end)?;
            let owner =
                owner.filter(|owner| owner.body_range.is_some_and(|body| body.encloses(holder)))?;
            let receiver_name = text(source, target.receiver)?;
            let receiver = *declarations
                .get(format!("{}.{receiver_name}", owner.qualified_name).as_str())?
                .as_ref()?;
            if receiver.syntax_element_kind != Some("variable_declarator")
                || !anchors.contains_key(&receiver.qualified_name)
                || !checked_receiver(
                    receiver,
                    target.range,
                    bindings.get(receiver_name)?,
                    writes.get(receiver_name).map(Vec::as_slice),
                )
            {
                return None;
            }
            let receiver_body = receiver.body_range?;
            let member_name = text(source, target.member?)?;
            let declaration = *declarations
                .get(format!("{}.{member_name}", receiver.qualified_name).as_str())?
                .as_ref()?;
            (ordinary_private_property(declaration, source)
                && anchors.contains_key(&declaration.qualified_name)
                && bodies.get(&declaration.item_range) == Some(&receiver_body))
            .then_some((holder, owner.qualified_name.as_str()))
        })
        .collect()
}

fn direct_member(source: &str, member: &StaticMember) -> bool {
    let Some(name) = member.member else {
        return false;
    };
    member.range.start == member.receiver.start
        && member.range.end == name.end
        && text(
            source,
            ByteRange {
                start: member.receiver.end,
                end: name.start,
            },
        )
        .is_some_and(|separator| separator.trim() == ".")
}

fn checked_receiver(
    declaration: &SyntaxSymbol,
    site: ByteRange,
    bindings: &[&StaticBinding],
    writes: Option<&[ByteRange]>,
) -> bool {
    let prevailing = bindings
        .iter()
        .filter(|binding| binding.scope.encloses(site))
        .min_by_key(|binding| binding.scope.end - binding.scope.start);
    prevailing.is_some_and(|binding| {
        binding.kind == StaticBindingKind::Local
            && declaration.name_range == Some(binding.name)
            && declaration.item_range.end <= site.start
            && binding.available_from <= site.start
            && !bindings
                .iter()
                .any(|other| other.name != binding.name && other.scope == binding.scope)
            && writes.is_none_or(|writes| {
                !writes
                    .iter()
                    .any(|write| binding.scope.encloses(*write) && write.start < site.start)
            })
    })
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
