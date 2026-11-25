//! One-hop method lookup through a locally declared generic `Deref` implementation.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;
use rift_syntax::{
    ByteRange, StaticBindingKind, StaticCall, StaticCallee, StaticInherentImpl, SyntaxSymbol,
};

use super::references::{ReceiverOwner, callee_parameters, source_text};
use super::{
    CallableReturn, Namespaces, SelfKind, generic_end, is_type_binding, is_value_binding, self_kind,
};
use crate::static_resolution::{Binding, File, Target, Work};

pub(super) type WrapperKey<'source> = (&'source str, ByteRange, bool);
type TraitHeaders<'source> = BTreeMap<ByteRange, Option<(ByteRange, &'source str, bool)>>;

pub(super) struct Implementation<'source> {
    parameters: &'source str,
    application: &'source str,
    target: Option<&'source str>,
}

pub(super) fn trait_headers<'source>(
    file: &File<'source>,
    work: &mut Work,
) -> Option<TraitHeaders<'source>> {
    let mut traits = BTreeMap::new();
    if file.document.facts.has_errors() || !file.document.source.contains("Deref") {
        return Some(traits);
    }
    for edge in file.facts.heritage_edges() {
        if !work.charge() {
            return None;
        }
        let name = file.text(edge.target)?.rsplit("::").next()?.trim();
        let mutable = match name {
            "Deref" => false,
            "DerefMut" => true,
            _ => continue,
        };
        traits
            .entry(edge.from)
            .and_modify(|entry| *entry = None)
            .or_insert(Some((edge.target, name, mutable)));
    }
    Some(traits)
}

impl<'source> Namespaces<'source> {
    pub(super) fn index_local_deref(
        &mut self,
        file: &File<'source>,
        implementation: &StaticInherentImpl,
        traits: &TraitHeaders<'source>,
        work: &mut Work,
    ) {
        let Some(Some((trait_range, name, mutable))) = traits.get(&implementation.type_path) else {
            return;
        };
        let Some(wrapper) = self.type_target(file, implementation.type_path, work) else {
            return;
        };
        if wrapper.file.path != file.document.path || wrapper.symbol.kind != "struct" {
            return;
        }
        let Some(mut wrapper_path) = wrapper.admitted_path() else {
            return;
        };
        if !wrapper_path.split_last().is_some_and(|(_, module)| {
            self.module(file, implementation.type_path.start, work) == Some(module)
        }) {
            return;
        }
        wrapper_path.push("Target".to_owned());
        let target = self.members.get(&wrapper_path).copied().flatten();
        let proof =
            self.deref_implementation(file, implementation, target, *trait_range, name, work);
        self.local_deref
            .entry((
                file.document.path.as_str(),
                wrapper.symbol.item_range,
                *mutable,
            ))
            .and_modify(|entry| *entry = None)
            .or_insert(proof);
    }

    fn deref_implementation(
        &self,
        file: &File<'source>,
        implementation: &StaticInherentImpl,
        target: Option<Target<'source>>,
        trait_range: ByteRange,
        name: &str,
        work: &mut Work,
    ) -> Option<Implementation<'source>> {
        let trait_text = file.text(trait_range)?;
        let written = self.spelled_path(file, trait_range.start, trait_text, false, work)?;
        let [library, module, trait_name] = written.as_slice() else {
            return None;
        };
        if !matches!(library.as_str(), "core" | "std") || module != "ops" || trait_name != name {
            return None;
        }
        let source = file.document.source;
        let prefix = between(source, implementation.range.start, trait_range.start)?;
        let (parameters, after) = generic_group(prefix.strip_prefix("impl")?, work)?;
        if !after.is_empty() {
            return None;
        }
        let applied = between(
            source,
            implementation.type_path.end,
            implementation.range.end,
        )?;
        let (application, arguments) = simple_arguments(applied.split_once('{')?.0, work)?;
        if !arguments.iter().all(|argument| {
            matches!(
                file.binding(argument, implementation.type_path.start, is_type_binding, work),
                Binding::Found(binding) if binding.scope == implementation.range
                    && binding.kind == StaticBindingKind::RustTypeParameter
            )
        }) {
            return None;
        }
        let target = if name == "Deref" {
            let target = target?;
            if target.file.path != file.document.path
                || !implementation.range.encloses(target.symbol.item_range)
                || !self.available(target)
            {
                return None;
            }
            Some(deref_target(file, target.symbol, &arguments, work)?)
        } else {
            None
        };
        Some(Implementation {
            parameters,
            application,
            target,
        })
    }

    pub(super) fn returned_deref_member(
        &self,
        file: &File<'source>,
        receiver: ByteRange,
        wrapper: Target<'source>,
        member: &str,
        work: &mut Work,
        depth: usize,
    ) -> Option<Target<'source>> {
        let depth = depth.checked_sub(1)?;
        let key = (wrapper.file.path.as_str(), wrapper.symbol.item_range);
        let selected = self.local_deref.get(&(key.0, key.1, false))?.as_ref()?;
        let mut path = wrapper.admitted_path()?;
        let crate_name = self.module(file, receiver.start, work)?.first()?;
        let origin = file.document.placement.origin();
        if path.first() != Some(crate_name) || wrapper.file.placement.origin() != origin {
            return None;
        }
        path.push(member.to_owned());
        if self.methods.contains_key(&path) || self.members.contains_key(&path) {
            return None;
        }
        let Binding::Found(receiver_binding) =
            file.binding(file.text(receiver)?, receiver.start, is_value_binding, work)
        else {
            return None;
        };
        let StaticBindingKind::RustValue { initializer } = receiver_binding.kind else {
            return None;
        };
        let call = self
            .initializers
            .get(&(file.document.path.as_str(), initializer))?
            .as_ref()?;
        if !matches!(call.callee, StaticCallee::Direct | StaticCallee::Qualified) {
            return None;
        }
        let callee = self.call_at_depth(file, call, work, depth)?;
        let name = callee.symbol.name_range?;
        let returned = self.return_owners.get(&(callee.file.path.as_str(), name))?;
        let _ = path.pop();
        let (_, wrapper_module) = path.split_last()?;
        let callee_path = callee.admitted_path()?;
        let (_, callee_module) = callee_path.split_last()?;
        if callee.file.path != wrapper.file.path
            || wrapper_module != callee_module
            || returned.owner.file.path != wrapper.file.path
            || returned.owner.symbol.item_range != wrapper.symbol.item_range
        {
            return None;
        }
        let parameter = matched_return_parameter(callee, returned, selected, work)?;
        let argument = typed_argument(file, callee, call, parameter, work)?;
        let ReceiverOwner::Target(target) =
            self.receiver_owner_at_depth(file, argument, work, depth)?
        else {
            return None;
        };
        if target.file.placement.origin() != origin
            || target.admitted_path()?.first() != Some(crate_name)
        {
            return None;
        }
        // The argument's own references are not known.
        let method = self.receiver_method_of(file, None, receiver.start, (target, member), work)?;
        match self_kind(method)? {
            SelfKind::Shared => Some(method),
            SelfKind::Unique => self
                .local_deref
                .get(&(key.0, key.1, true))?
                .as_ref()
                .filter(|mutable| {
                    equal_parameters(mutable.application, selected.application)
                        && equal_parameters(mutable.parameters, selected.parameters)
                })
                .map(|_| method),
            _ => None,
        }
    }
}

fn deref_target<'source>(
    file: &File<'source>,
    symbol: &SyntaxSymbol,
    arguments: &[&'source str],
    work: &mut Work,
) -> Option<&'source str> {
    if symbol.kind != "type_alias" || file.conditional(symbol.item_range, work) {
        return None;
    }
    let (declaration, value) = file
        .text(symbol.item_range)?
        .strip_suffix(';')?
        .split_once('=')?;
    if declaration.split_whitespace().ne(["type", "Target"]) {
        return None;
    }
    let target = arguments
        .iter()
        .copied()
        .find(|argument| *argument == value.trim())?;
    file.facts
        .references()
        .iter()
        .any(|reference| {
            work.charge()
                && reference.role == ReferenceRole::Type
                && symbol.item_range.encloses(reference.target)
                && file.text(reference.target) == Some(target)
        })
        .then_some(target)
}

fn generic_group<'source>(
    text: &'source str,
    work: &mut Work,
) -> Option<(&'source str, &'source str)> {
    let text = text.trim_start();
    if !text.starts_with('<') {
        return None;
    }
    let end = generic_end(text, work)?;
    let group = &text[1..end];
    if group.contains('/') || group.contains('"') || group.contains('{') {
        return None;
    }
    Some((group, text[end + 1..].trim()))
}

fn simple_arguments<'source>(
    text: &'source str,
    work: &mut Work,
) -> Option<(&'source str, Vec<&'source str>)> {
    const ARGUMENTS_MAX: usize = 8;
    let (group, after) = generic_group(text, work)?;
    if !after.is_empty() {
        return None;
    }
    let arguments: Vec<_> = group
        .split(',')
        .map(str::trim)
        .take(ARGUMENTS_MAX + 1)
        .collect();
    if arguments.len() > ARGUMENTS_MAX {
        return None;
    }
    let mut unique = BTreeSet::new();
    arguments
        .iter()
        .all(|argument| {
            let mut chars = argument.chars();
            chars
                .next()
                .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
                && chars.all(|ch| ch.is_alphanumeric() || ch == '_')
                && unique.insert(*argument)
        })
        .then_some((group, arguments))
}

fn equal_parameters(left: &str, right: &str) -> bool {
    left.chars()
        .filter(|ch| !ch.is_whitespace())
        .eq(right.chars().filter(|ch| !ch.is_whitespace()))
}

fn between(source: &str, start: u64, end: u64) -> Option<&str> {
    source_text(source, ByteRange { start, end })
}

fn matched_return_parameter<'source>(
    callee: Target<'source>,
    returned: &CallableReturn<'source>,
    selected: &Implementation<'source>,
    work: &mut Work,
) -> Option<&'source str> {
    let source = callee.file.source;
    let name = callee.symbol.name_range?;
    let header_end = callee.symbol.body_range?.start;
    let (parameters, after) = generic_group(between(source, name.end, header_end)?, work)?;
    if !after.starts_with('(') || !equal_parameters(parameters, selected.parameters) {
        return None;
    }
    let before_return = between(source, name.end, returned.written.start)?;
    if !before_return.trim_end().ends_with("->") {
        return None;
    }
    let applied = between(source, returned.written.end, header_end)?;
    let (application, after) = generic_group(applied, work)?;
    if !after.is_empty() || !equal_parameters(application, selected.application) {
        return None;
    }
    selected.target
}

fn typed_argument(
    file: &File<'_>,
    callee: Target<'_>,
    call: &StaticCall,
    parameter: &str,
    work: &mut Work,
) -> Option<ByteRange> {
    let parameters = callee_parameters(callee, false)?;
    let mut positions = parameters
        .iter()
        .enumerate()
        .filter_map(|(position, binding)| {
            let StaticBindingKind::RustReceiver { type_path } = binding.kind else {
                return None;
            };
            (source_text(callee.file.source, type_path)? == parameter).then_some(position)
        });
    let (Some(index), None) = (positions.next(), positions.next()) else {
        return None;
    };
    let (_, arguments) = file
        .facts
        .closure_calls()
        .iter()
        .find(|(range, _)| work.charge() && *range == call.range)?;
    let first = arguments.first()?;
    if arguments.len() != parameters.len()
        || between(file.document.source, call.callee_range.end, first.start)?.trim() != "("
    {
        return None;
    }
    arguments.get(index).copied()
}
