//! Authored Rust invocations with a proved root export or textual definition.

use std::{cmp::Reverse, collections::BTreeMap};

use rift_core::ReferenceRole;
use rift_protocol::read::EdgeKind;
use rift_syntax::{StaticRustMacroCall, StaticRustMacroDefinition};

use super::Namespaces;
use crate::static_resolution::{File, Index, StaticResolution, Target, Work};

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn rust_root_macro_calls(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for call in file.facts.rust_root_macro_calls() {
            if !work.charge() {
                return;
            }
            if file.conditional(call.range, work) {
                continue;
            }
            let occurrence = self
                .rust
                .root_macro_target(file, *call, work)
                .and_then(|target| {
                    self.occurrence(file, call.range, call.name, ReferenceRole::Call, target)
                });
            result.record_kind(occurrence, EdgeKind::Calls);
        }
    }
    pub(in crate::static_resolution) fn rust_textual_macro_calls(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        if file.document.facts.has_errors() || file.facts.rust_macro_definitions().is_empty() {
            return;
        }
        let mut definitions = BTreeMap::<&str, Vec<StaticRustMacroDefinition>>::new();
        for definition in file.facts.rust_macro_definitions() {
            if !work.charge() {
                return;
            }
            if let Some(name) = file.text(definition.name) {
                definitions.entry(name).or_default().push(*definition);
            }
        }
        for call in file.facts.rust_textual_macro_calls() {
            if !work.charge() {
                return;
            }
            let Some(definitions) = file.text(call.name).and_then(|name| definitions.get(name))
            else {
                continue;
            };
            if file.conditional(call.range, work) {
                continue;
            }
            let occurrence = self
                .rust
                .textual_macro_target(file, *call, definitions, work)
                .and_then(|target| {
                    self.occurrence(file, call.range, call.name, ReferenceRole::Call, target)
                });
            result.record_kind(occurrence, EdgeKind::Calls);
        }
    }
}

impl<'source> Namespaces<'source> {
    fn textual_macro_target(
        &self,
        file: &File<'source>,
        call: StaticRustMacroCall,
        definitions: &[StaticRustMacroDefinition],
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let position = call.range.start;
        self.module(file, position, work)?;
        let rank = |definition: &StaticRustMacroDefinition| {
            (
                Reverse(definition.scope.end - definition.scope.start),
                definition.item.end,
            )
        };
        let mut selected = None;
        for definition in definitions {
            if !work.charge() {
                return None;
            }
            if definition.scope.encloses(call.range)
                && definition.item.end <= position
                && !file.inactive(definition.item, work)
                && selected.is_none_or(|current| rank(definition) > rank(current))
            {
                selected = Some(definition);
            }
        }
        let definition = selected?;
        if file.conditional(definition.item, work) {
            return None;
        }
        self.module(file, definition.item.start, work)?;
        for &(item, scope) in file.facts.rust_macro_barriers() {
            if !work.charge()
                || (scope.encloses(call.range)
                    && item.start < position
                    && item.end > definition.item.start
                    && !file.inactive(item, work))
            {
                return None;
            }
        }
        file.target_at(definition.name).filter(|target| {
            target.symbol.kind == "macro"
                && target.symbol.item_range == definition.item
                && self.available(*target)
        })
    }

    fn root_macro_target(
        &self,
        file: &File<'source>,
        call: StaticRustMacroCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let crate_name = self.module(file, call.range.start, work)?.first()?;
        self.crate_roots.get(crate_name)?.as_ref()?;
        let path = [crate_name.clone(), file.text(call.name)?.to_owned()];
        let target = self
            .exported_macros
            .get(path.as_slice())
            .copied()
            .flatten()?;
        let definition = self.source_files.get(target.file.path.as_str())?;
        let defining_crate = self
            .module(definition, target.symbol.item_range.start, work)?
            .first()?;
        (defining_crate == crate_name && self.available(target)).then_some(target)
    }
}
