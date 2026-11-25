//! Authored value-path terminals in expressions and patterns.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::{ByteRange, StaticBindingKind, StaticRustValueRead};

use super::{Namespace, Namespaces, is_value_binding, path_segments};
use crate::static_resolution::{Binding, File, Index, StaticResolution, Target, Work};

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn rust_value_reads(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        if file.facts.rust_value_reads().is_empty() {
            return;
        }
        let mut local_values: BTreeMap<&str, Vec<ByteRange>> = BTreeMap::new();
        for &(name, scope) in file.facts.rust_local_values() {
            if !work.charge() {
                return;
            }
            if !file.inactive(name, work)
                && let Some(name) = file.text(name)
            {
                local_values.entry(name).or_default().push(scope);
            }
        }
        for read in file.facts.rust_value_reads() {
            if !work.charge() {
                return;
            }
            let position = read.range.start;
            if file.conditional(read.range, work) || file.blocked(position, work) {
                continue;
            }
            let Some((text, parts)) = file
                .text(read.range)
                .and_then(|text| Some((text, path_segments(text, work)?)))
            else {
                result.record_kind(None, EdgeKind::Reads);
                continue;
            };
            let bare = match parts.as_slice() {
                [name] => {
                    let binding = file.binding(name, position, shadows_value, work);
                    let narrowed = match &binding {
                        Binding::Found(binding) => Some(binding.scope),
                        _ => None,
                    };
                    // A hoisted block item shadows a module value or import. An established
                    // binding in a strictly inner block still supplies its own value.
                    if local_values.get(*name).is_some_and(|scopes| {
                        scopes.iter().any(|scope| {
                            !work.charge()
                                || (scope.contains(position)
                                    && narrowed.is_none_or(|inner| {
                                        *scope == inner || !scope.encloses(inner)
                                    }))
                        })
                    }) {
                        continue;
                    }
                    Some(binding)
                }
                _ => None,
            };
            let target = match bare.as_ref() {
                Some(Binding::Found(binding)) if binding.kind == StaticBindingKind::Local => {
                    // An authored function declaration is also stored as a lexical binding.
                    let Some(function) = Namespaces::local_callable(file, binding.name, work)
                        .filter(|target| {
                            target.symbol.kind == "function" && file.is_module_item(target.symbol)
                        })
                    else {
                        continue;
                    };
                    Some(function)
                }
                Some(Binding::Found(binding))
                    if !matches!(binding.kind, StaticBindingKind::RustUse { .. }) =>
                {
                    continue;
                }
                _ => self.rust.value_read_target(file, *read, &parts, bare, work),
            };
            let occurrence = target.and_then(|target| {
                let terminal = text.as_bytes().subslice_range(parts.last()?.as_bytes())?;
                let range = ByteRange {
                    start: position.checked_add(u64::try_from(terminal.start).ok()?)?,
                    end: position.checked_add(u64::try_from(terminal.end).ok()?)?,
                };
                self.occurrence(file, range, range, ReferenceRole::Read, target)
            });
            result.record_kind(occurrence, EdgeKind::Reads);
        }
    }
}

impl<'source> Namespaces<'source> {
    fn value_read_target(
        &self,
        file: &File<'source>,
        read: StaticRustValueRead,
        parts: &[&str],
        bare: Option<Binding<'source>>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let position = read.range.start;
        let head = *parts.first()?;
        let path = if let Some(binding) = bare {
            match binding {
                Binding::Found(binding) => {
                    let StaticBindingKind::RustUse { path } = binding.kind else {
                        return None;
                    };
                    if file.conditional(path, work) {
                        return None;
                    }
                    let imported = file.use_parts(path, work)?;
                    let imported = imported.iter().map(String::as_str).collect::<Vec<_>>();
                    self.absolute_path(file, path.start, &imported, work)?
                }
                Binding::Missing => {
                    let mut path = self.module(file, position, work)?.to_vec();
                    path.push(head.to_owned());
                    path
                }
                Binding::Blocked => return None,
            }
        } else {
            if head == "Self"
                || (!matches!(head, "crate" | "self" | "super")
                    && Self::local_named(file, position, head, work, |target| {
                        target.symbol.kinds.contains(&SymbolKind::Type)
                            || target.symbol.kinds.contains(&SymbolKind::Module)
                    })
                    .is_some())
            {
                return None;
            }
            self.resolved_path(file, position, parts, work)?
        };
        if let Some(target) = self.lookup(file, position, &path, Namespace::ConstantOrStatic, work)
        {
            return (!read.pattern || target.symbol.kind == "constant").then_some(target);
        }
        if !read.pattern
            && let Some(target) = self
                .lookup(file, position, &path, Namespace::Value, work)
                .filter(|target| target.symbol.kind == "function")
        {
            return Some(target);
        }
        let (name, prefix) = path.split_last()?;
        if let Some(owner) = self
            .lookup(file, position, prefix, Namespace::Type, work)
            .filter(|owner| owner.symbol.kind == "enum")
        {
            let mut member = owner.admitted_path()?;
            member.push(name.clone());
            let variant = self.values.get(&member).copied().flatten()?;
            return (variant.symbol.kind == "variant"
                && self.available(variant)
                && (read.pattern || value_constructor(variant)))
            .then_some(variant);
        }
        let target = self
            .lookup(file, position, &path, Namespace::Type, work)
            .filter(|target| target.symbol.kind == "struct" && value_constructor(*target))?;
        // Tuple fields have their own visibility. Only the containing module and its
        // descendants can name the constructor without proving those fields' visibility.
        if target.symbol.body_range.is_some()
            && !self.module(file, position, work)?.starts_with(prefix)
        {
            return None;
        }
        Some(target)
    }
}

fn shadows_value(kind: StaticBindingKind) -> bool {
    is_value_binding(kind) || kind == StaticBindingKind::RustTypeParameter
}

fn value_constructor(target: Target<'_>) -> bool {
    target.symbol.body_range.is_none_or(|body| {
        usize::try_from(body.start)
            .ok()
            .and_then(|start| target.file.source.as_bytes().get(start))
            == Some(&b'(')
    })
}
