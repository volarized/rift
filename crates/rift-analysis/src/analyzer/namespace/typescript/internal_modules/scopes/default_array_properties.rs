//! Physical property scopes in a written default-exported array.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::SymbolIdentity;
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticBodyMember, SyntaxExportKind, SyntaxSymbol,
};

use super::{
    FileScopes, SelectedFile, binding_name, literal_property_identity, local_group_identity,
    written_array,
};

type Property<'source> = (&'source SyntaxSymbol, &'source StaticBodyMember);

impl<'source> FileScopes<'source> {
    /// Keeps repeated keys and their children under their own written array slot.
    pub(in super::super) fn place_default_array_properties(
        &self,
        file: &SelectedFile<'source>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        scoped: &mut ScopedIdentityAnchors,
    ) {
        if withheld.is_empty()
            || file.syntax.language().name != "javascript"
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
        {
            return;
        }
        let Some(array) = default_array(file) else {
            return;
        };
        let mut named = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        let mut written = BTreeMap::new();
        let mut counts = BTreeMap::new();
        for symbol in file.syntax.symbols() {
            let name = symbol.qualified_name.as_str();
            if withheld.contains(name) && anchors.contains_key(name) {
                named.entry(name).or_default().push(symbol);
            }
            let Some(&member) = self.body_members.get(&symbol.item_range) else {
                continue;
            };
            if let Some(key) = authored_key(file.source, symbol) {
                *counts.entry((member.body, key)).or_insert(0_usize) += 1;
                written.insert(symbol.item_range, (symbol, member));
            }
        }
        let mut occupied = anchors
            .values()
            .chain(scoped.values().flat_map(BTreeMap::values))
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut holders = BTreeMap::new();
        for (name, group) in named {
            let Some(original) = anchors.get(name) else {
                continue;
            };
            let mut bodies = BTreeSet::new();
            let verified = group.iter().all(|symbol| {
                symbol.container.is_none()
                    && written
                        .get(&symbol.item_range)
                        .is_some_and(|(property, member)| {
                            member.holder == array
                                && array.encloses(member.body)
                                && is_unique_key(file.source, property, member, &counts)
                                && bodies.insert(member.body)
                        })
            });
            if !verified || bodies.len() < 2 || scoped.contains_key(name) {
                continue;
            }
            let Some(ids) = group
                .iter()
                .map(|symbol| {
                    let id = local_group_identity(original, symbol.declaration_ordinal)?;
                    (!occupied.contains(&id)).then_some((*symbol, id))
                })
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            for (symbol, id) in ids {
                remember_holder(file.source, symbol, &id, &mut holders);
                occupied.insert(id.clone());
                scoped
                    .entry(name.to_owned())
                    .or_default()
                    .insert(symbol.declaration_ordinal, id);
            }
        }
        place_children(
            file.source,
            written.into_values(),
            &counts,
            &mut holders,
            &mut occupied,
            scoped,
        );
    }
}

type Holders<'source> = BTreeMap<ByteRange, (&'source str, SymbolIdentity)>;

fn remember_holder<'source>(
    source: &str,
    property: &'source SyntaxSymbol,
    id: &SymbolId,
    holders: &mut Holders<'source>,
) {
    if property.syntax_element_kind != Some("pair") {
        return;
    }
    let Ok(owner) = SymbolIdentity::parse(id.as_str()) else {
        return;
    };
    let Some(value) = property
        .body_range
        .filter(|value| property.item_range.encloses(*value))
    else {
        return;
    };
    let holder = if written_array(source, value) {
        value
    } else if range_text(source, value)
        .is_some_and(|value| value.starts_with('{') && value.ends_with('}'))
    {
        property.item_range
    } else {
        return;
    };
    holders.insert(holder, (property.qualified_name.as_str(), owner));
}

fn place_children<'source>(
    source: &str,
    written: impl Iterator<Item = Property<'source>>,
    counts: &BTreeMap<(ByteRange, String), usize>,
    holders: &mut Holders<'source>,
    occupied: &mut BTreeSet<SymbolId>,
    scoped: &mut ScopedIdentityAnchors,
) {
    // Every written pair starts before the properties of its literal value.
    for (property, member) in written {
        let Some((parent, owner)) = holders.get(&member.holder) else {
            continue;
        };
        if property.container.as_deref() != Some(*parent)
            || !is_unique_key(source, property, member, counts)
            || scoped
                .get(&property.qualified_name)
                .is_some_and(|group| group.contains_key(&property.declaration_ordinal))
        {
            continue;
        }
        let Some(id) = literal_property_identity(owner, &property.name) else {
            continue;
        };
        if occupied.insert(id.clone()) {
            remember_holder(source, property, &id, holders);
            scoped
                .entry(property.qualified_name.clone())
                .or_default()
                .insert(property.declaration_ordinal, id);
        }
    }
}

fn default_array(file: &SelectedFile<'_>) -> Option<ByteRange> {
    let mut defaults = file.syntax.export_bindings()?.iter().filter(|binding| {
        binding.kind == SyntaxExportKind::Default
            && binding.container.is_none()
            && binding.source.is_none()
    });
    let binding = defaults.next()?;
    let value = binding.local?;
    (defaults.next().is_none()
        && binding.range.encloses(value)
        && written_array(file.source, value))
    .then_some(value)
}

fn is_unique_key(
    source: &str,
    property: &SyntaxSymbol,
    member: &StaticBodyMember,
    counts: &BTreeMap<(ByteRange, String), usize>,
) -> bool {
    authored_key(source, property).is_some_and(|key| counts.get(&(member.body, key)) == Some(&1))
}

fn authored_key(source: &str, property: &SyntaxSymbol) -> Option<String> {
    let name = property.name_range?;
    (property.kind == "property"
        && matches!(
            property.syntax_element_kind,
            Some("pair" | "shorthand_property_identifier")
        )
        && range_text(source, name)? == property.name
        && !property.name.starts_with('['))
    .then(|| binding_name(source, name))
    .flatten()
}

fn range_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
