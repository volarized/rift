//! Authored Rust invocations and named imports with a proved macro declaration.

use std::{cmp::Reverse, collections::BTreeMap};

use rift_core::ReferenceRole;
use rift_protocol::read::EdgeKind;
use rift_syntax::{ByteRange, StaticRustMacroDefinition};

use super::{Namespaces, namespace_visible};
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
            let occurrence = file.text(call.name).and_then(|name| {
                let target = self.rust.root_macro_target(file, call.range, name, work)?;
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
            let Some(name) = file.text(call.name) else {
                continue;
            };
            let Some(definitions) = definitions.get(name) else {
                continue;
            };
            if file.conditional(call.range, work) {
                continue;
            }
            let occurrence = self
                .rust
                .textual_macro_target(file, call.range, name, definitions, work)
                .and_then(|target| {
                    self.occurrence(file, call.range, call.name, ReferenceRole::Call, target)
                });
            result.record_kind(occurrence, EdgeKind::Calls);
        }
    }
    pub(in crate::static_resolution) fn rust_macro_imports(
        &self,
        file: &File<'source>,
        sites: &[ByteRange],
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for &site in sites {
            if !work.charge() {
                return;
            }
            let occurrence = self
                .rust
                .macro_import_target(file, site, work)
                .and_then(|target| self.rust_import_occurrence(file, site, target, work));
            if work.exhausted {
                return;
            }
            let Some(occurrence) = occurrence else {
                continue;
            };
            let Some(remaining) = result.omitted.get_mut(&EdgeKind::Imports) else {
                return;
            };
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                result.omitted.remove(&EdgeKind::Imports);
            }
            result.unresolved_sites = result.unresolved_sites.saturating_sub(1);
            result.record_kind(Some(occurrence), EdgeKind::Imports);
        }
    }
}

impl<'source> Namespaces<'source> {
    fn textual_macro_target(
        &self,
        file: &File<'source>,
        site: ByteRange,
        name: &str,
        definitions: &[StaticRustMacroDefinition],
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let position = site.start;
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
            if definition.scope.encloses(site)
                && definition.item.end <= position
                && file.text(definition.name) == Some(name)
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
                || (scope.encloses(site)
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

    fn macro_import_target(
        &self,
        file: &File<'source>,
        written: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if file.conditional(written, work) || file.blocked(written.start, work) {
            return None;
        }
        let parts = file.use_parts(written, work)?;
        if let [name] = parts.as_slice() {
            return self.textual_macro_import(file, written, name, work);
        }
        let [head, .., name] = parts.as_slice() else {
            return None;
        };
        if head != "crate" {
            return None;
        }
        if parts.len() == 2
            && let Some(target) = self.root_macro_target(file, written, name, work)
        {
            return Some(target);
        }

        let source = self.module(file, written.start, work)?;
        let mut path = source.get(..1)?.to_vec();
        path.extend(parts.into_iter().skip(1));
        let reexport = self.exports.get(&path)?.as_ref()?;
        if !namespace_visible(source, &reexport.module, reexport.visibility)
            || !self.module_path_visible(source, &reexport.module, work)
        {
            return None;
        }
        let exporting_file = self.source_files.get(reexport.file)?;
        let original = exporting_file.use_parts(reexport.written, work)?;
        let [original] = original.as_slice() else {
            return None;
        };
        let (name, namespace) = reexport.target.split_last()?;
        if namespace != reexport.module.as_slice() || name != original {
            return None;
        }
        self.textual_macro_import(exporting_file, reexport.written, original, work)
    }

    fn textual_macro_import(
        &self,
        file: &File<'source>,
        written: ByteRange,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if file.document.facts.has_errors()
            || file.text(written) != Some(name)
            || file.conditional(written, work)
            || file.blocked(written.start, work)
        {
            return None;
        }
        self.textual_macro_target(
            file,
            written,
            name,
            file.facts.rust_macro_definitions(),
            work,
        )
    }

    fn root_macro_target(
        &self,
        file: &File<'source>,
        written: ByteRange,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let crate_name = self.module(file, written.start, work)?.first()?;
        self.crate_roots.get(crate_name)?.as_ref()?;
        let path = [crate_name.clone(), name.to_owned()];
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
