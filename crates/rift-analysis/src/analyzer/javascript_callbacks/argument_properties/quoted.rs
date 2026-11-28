//! Quoted argument properties below an already retained physical callback.

use std::collections::{BTreeMap, BTreeSet};

use rift_error::RiftError;
use rift_protocol::{
    identity::SymbolOwner,
    read::{EdgeKind, SymbolId, SymbolKind},
};
use rift_syntax::{ByteRange, SyntaxSymbol};

use super::{
    ArgumentProperty, CallbackFile, argument_properties, argument_property_identity, source_text,
    written_callback,
};
use crate::analyzer::{AnalyzedFile, ExactPackageLimits, Records};

impl Records {
    pub(in crate::analyzer) fn retain_quoted_callback_argument_properties(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        limits: ExactPackageLimits,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated
            || !matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm")
        {
            return Ok(());
        }
        let files = self.ecmascript_callback_files(analyzed, EdgeKind::Calls);
        let mut ranges = BTreeMap::<_, BTreeSet<_>>::new();
        let mut callbacks = BTreeMap::new();
        for row in &self.declarations {
            let range = ByteRange {
                start: row.range.start,
                end: row.range.end,
            };
            ranges.entry(row.unit.clone()).or_default().insert(range);
            if matches!(
                row.syntax_element_kind.as_deref(),
                Some("arrow_function" | "function_expression")
            ) && row.source_complete
                && row.symbol.as_str().contains("/$callback%23")
            {
                callbacks
                    .entry((row.unit.clone(), range))
                    .and_modify(|prior| *prior = None)
                    .or_insert(Some(row.symbol.clone()));
            }
        }
        if callbacks.is_empty() {
            return Ok(());
        }
        let mut occupied = self
            .objects
            .iter()
            .filter_map(|row| row.id.clone())
            .collect();
        for file in files
            .values()
            .filter(|file| file.file.file.syntax().language().name == "javascript")
        {
            let Some(placed) = ranges.get(&file.unit) else {
                continue;
            };
            let callback_owner = |span| written_callback(file, span);
            let mut roots = Vec::new();
            let mut checked = BTreeMap::new();
            for (property, name, callback, _) in
                argument_properties(file, callback_owner, placed, true)
            {
                let Some(Some(id)) = callbacks.get(&(file.unit.clone(), callback.range)) else {
                    continue;
                };
                let supported = checked
                    .entry(callback.range)
                    .or_insert_with(|| self.retained_callback(file, callback, id));
                if !*supported {
                    continue;
                }
                let Some(identity) = argument_property_identity(owner, property, id)? else {
                    continue;
                };
                let key = SymbolId(identity.wire_identity());
                let earlier = self.declarations.len();
                self.publish_callback_argument_properties(
                    owner,
                    limits,
                    [ArgumentProperty {
                        file,
                        property,
                        name,
                        callback: id.clone(),
                    }],
                    &mut occupied,
                )?;
                if self.declarations[earlier..].iter().any(|row| {
                    row.symbol == key
                        && row.unit == file.unit
                        && row.source_complete
                        && row.range.start == property.range.start
                        && row.range.end == property.range.end
                }) {
                    roots.push((property, key));
                }
            }
            self.publish_quoted_argument_children(
                owner,
                limits,
                file,
                &roots,
                placed,
                &mut occupied,
            )?;
        }
        Ok(())
    }

    fn publish_quoted_argument_children(
        &mut self,
        owner: &SymbolOwner,
        limits: ExactPackageLimits,
        file: &CallbackFile<'_>,
        roots: &[(&SyntaxSymbol, SymbolId)],
        placed: &BTreeSet<ByteRange>,
        occupied: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        if roots.is_empty() || self.symbols_truncated {
            return Ok(());
        }
        let source = file.file.file.source();
        let Some(facts) = file.file.file.syntax().static_facts() else {
            return Ok(());
        };
        let by_body = roots
            .iter()
            .filter_map(|(parent, id)| {
                let body = parent.body_range?;
                source_text(source, body)
                    .is_some_and(|value| value.starts_with('{') && value.ends_with('}'))
                    .then_some((body, (*parent, id)))
            })
            .collect::<BTreeMap<_, _>>();
        let members = facts
            .body_members()
            .iter()
            .filter(|member| by_body.contains_key(&member.body))
            .map(|member| (member.member, member))
            .collect::<BTreeMap<_, _>>();
        let mut candidates = BTreeMap::new();
        for property in file.file.file.syntax().symbols() {
            let (Some(member), Some(name)) =
                (members.get(&property.item_range), property.name_range)
            else {
                continue;
            };
            let Some((parent, id)) = by_body.get(&member.body) else {
                continue;
            };
            if member.holder != parent.item_range
                || property.container.as_deref() != Some(parent.qualified_name.as_str())
                || property.kind != "property"
                || property.kinds.contains(&SymbolKind::Public)
                || property.syntax_element_kind != Some("pair")
                || source_text(source, name) != Some(property.name.as_str())
                || property.name.contains(['\'', '"', '`', '['])
                || placed.contains(&property.range)
            {
                continue;
            }
            candidates
                .entry((member.body, property.name.as_str()))
                .and_modify(|prior| *prior = None)
                .or_insert_with(|| {
                    Some(ArgumentProperty {
                        file,
                        property,
                        name,
                        callback: (*id).clone(),
                    })
                });
        }
        self.publish_callback_argument_properties(
            owner,
            limits,
            candidates.into_values().flatten(),
            occupied,
        )
    }
}

pub(super) fn is_simple_quoted_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let (Some(quote @ (b'\'' | b'"')), Some(end)) = (bytes.next(), bytes.next_back()) else {
        return false;
    };
    end == quote
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || matches!(first, b'_' | b'$'))
        && bytes
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, b'_' | b'$'))
}

pub(super) fn unquoted_name(name: &str) -> &str {
    if is_simple_quoted_name(name) {
        &name[1..name.len() - 1]
    } else {
        name
    }
}
