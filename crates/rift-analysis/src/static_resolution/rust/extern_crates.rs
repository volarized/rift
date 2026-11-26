//! Resolve paths whose authored head is an explicit extern crate binding.

use std::collections::BTreeSet;

use rift_core::ReferenceRole;
use rift_protocol::read::EdgeKind;
use rift_provider::NormalizedTarget;
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, StaticCall};

use super::super::external::RUST_RUNTIME_CRATES;
use super::super::{Bindings, ExternalOccurrence, Index, StaticResolution};
use super::{Binding, ExternCallee, File, Namespace, Namespaces, Target, Work, is_type_binding};
use crate::EnclosingDefinitions;

impl<'source> File<'source> {
    pub(in crate::static_resolution) fn index_rust_runtime_aliases(
        &mut self,
        runtime_aliases: Bindings<'source>,
    ) {
        self.rust_runtime_aliases = runtime_aliases
            .into_iter()
            .filter(|(name, bindings)| {
                bindings.iter().any(|binding| {
                    matches!(
                        binding.kind,
                        StaticBindingKind::RustExternCrate { crate_name }
                            if self.text(crate_name) != Some(*name)
                    )
                })
            })
            .map(|(name, bindings)| {
                let scopes = bindings
                    .into_iter()
                    .map(|binding| (binding.scope.start, binding.scope.end, binding));
                (name, EnclosingDefinitions::new(scopes))
            })
            .collect();
    }
}

impl<'source> Index<'source> {
    /// Established calls and earlier optional references retain their work and admission order.
    pub(in crate::static_resolution) fn resolve_rust_extern_aliases(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let mut candidates = std::mem::take(&mut result.rust_extern_candidates);
        let start = (result.occurrences.len(), result.external.len());
        for path in &self.rust_extern_files {
            if work.exhausted {
                break;
            }
            let Some(file) = self.files.get(path) else {
                continue;
            };
            let unit = file.document.placement.unit();
            let mut resolved = BTreeSet::new();
            if let Some(calls) = candidates.remove(unit) {
                for call in calls {
                    if !work.charge() {
                        break;
                    }
                    if self.resolve_extern_call(file, &call, work, result) {
                        resolved.insert(call.range);
                    }
                }
            }
            if !resolved.is_empty() {
                if let Some(coverage) = result.edge_calls.get_mut(unit) {
                    coverage
                        .unresolved
                        .retain(|range| !resolved.contains(range));
                }
                if let Some(remaining) = result.omitted.get_mut(&EdgeKind::Calls) {
                    let recovered = u64::try_from(resolved.len()).unwrap_or(u64::MAX);
                    *remaining = remaining.saturating_sub(recovered);
                    if *remaining == 0 {
                        result.omitted.remove(&EdgeKind::Calls);
                    }
                }
                result.unresolved_sites = result.unresolved_sites.saturating_sub(resolved.len());
            }
            for &binding in &file.rust_extern_aliases {
                if !work.charge() {
                    break;
                }
                if let Some(occurrence) = self.extern_import(file, binding, work) {
                    result.external.push(occurrence);
                }
            }
        }
        if start != (result.occurrences.len(), result.external.len()) {
            result.rust_extern_alias_start = Some(start);
        }
    }

    fn resolve_extern_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> bool {
        match self.rust.extern_callee(file, call, work) {
            Some(ExternCallee::Local(target)) => {
                let Some(occurrence) = self.occurrence(
                    file,
                    call.range,
                    call.callee_range,
                    ReferenceRole::Call,
                    target,
                ) else {
                    return false;
                };
                if !matches!(occurrence.target, NormalizedTarget::Symbol(_)) {
                    return false;
                }
                result.occurrences.push(occurrence);
            }
            Some(ExternCallee::Runtime(path)) => {
                let Some(occurrence) = self.rust_external_path(file, &path).and_then(|target| {
                    Self::external_occurrence(
                        file,
                        call.range,
                        call.callee_range,
                        ReferenceRole::Call,
                        target,
                    )
                }) else {
                    return false;
                };
                result.external.push(occurrence);
            }
            None => return false,
        }
        true
    }

    fn extern_import(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<ExternalOccurrence> {
        let StaticBindingKind::RustExternCrate { crate_name } = binding.kind else {
            return None;
        };
        let name = file.text(crate_name)?;
        if !RUST_RUNTIME_CRATES.contains(&name)
            || file.conditional(crate_name, work)
            || crate::analyzer::namespace::rust_file_scope(file.document.source)
                != Some(binding.scope)
        {
            return None;
        }
        let target = self.rust_external_path(file, &[name.to_owned()])?;
        let mut occurrence =
            Self::external_occurrence(file, crate_name, crate_name, ReferenceRole::Import, target)?;
        occurrence.edge_from = Some(file.document.placement.module()?.clone());
        Some(occurrence)
    }
}

impl<'source> Namespaces<'source> {
    fn extern_callee(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<ExternCallee<'source>> {
        if file.conditional(call.range, work) || file.blocked(call.range.start, work) {
            return None;
        }
        let parts = super::path_segments(file.text(call.callee_range)?, work)?;
        let (head, tail) = parts.split_first()?;
        if tail.is_empty() || matches!(*head, "crate" | "self" | "super" | "Self") {
            return None;
        }
        let position = call.range.start;
        let module = self.module(file, position, work)?;
        let Binding::Found(binding) = self.extern_binding(
            file,
            file.rust_extern_aliases.iter().copied(),
            head,
            module,
            position,
            work,
        ) else {
            return None;
        };
        let StaticBindingKind::RustExternCrate { crate_name } = binding.kind else {
            return None;
        };
        if !self.visible_extern(file, binding, crate_name, module, position, work) {
            return None;
        }
        let spelling = file.text(crate_name)?;
        let mut path = Vec::with_capacity(parts.len());
        if spelling == "self" && crate_name != binding.name {
            path.push(module.first()?.clone());
            path.extend(tail.iter().map(|part| (*part).to_owned()));
            return self
                .extern_local_callee(file, position, &path, work)
                .map(ExternCallee::Local);
        }
        if !RUST_RUNTIME_CRATES.contains(&spelling) {
            return None;
        }
        path.push(spelling.to_owned());
        path.extend(tail.iter().map(|part| (*part).to_owned()));
        Some(ExternCallee::Runtime(path))
    }

    /// An exact written extern binding owns a conflicting implicit runtime spelling.
    pub(super) fn hides_implicit_runtime(
        &self,
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> bool {
        let Some(bindings) = file.rust_runtime_aliases.get(name) else {
            return false;
        };
        let Some(scoped) = position
            .checked_add(1)
            .and_then(|end| bindings.containing(position, end))
        else {
            return false;
        };
        let mut scoped = scoped.copied().peekable();
        if scoped.peek().is_none() {
            return false;
        }
        let Some(module) = self.module(file, position, work) else {
            return false;
        };
        match self.extern_binding(file, scoped, name, module, position, work) {
            Binding::Found(binding) => matches!(
                binding.kind,
                StaticBindingKind::RustExternCrate { crate_name } if file.text(crate_name) != Some(name)
            ),
            Binding::Missing => false,
            Binding::Blocked => true,
        }
    }

    fn visible_extern(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        crate_name: ByteRange,
        module: &[String],
        position: u64,
        work: &mut Work,
    ) -> bool {
        let Some(spelling) = file.text(binding.name) else {
            return false;
        };
        if file.conditional(crate_name, work) {
            return false;
        }
        match file.binding(spelling, position, is_type_binding, work) {
            Binding::Missing => {}
            Binding::Found(other)
                if other.scope != binding.scope && other.scope.encloses(binding.scope) => {}
            _ => return false,
        }
        if file.facts.rust_local_types().iter().any(|&(name, scope)| {
            !work.charge() || (scope.contains(position) && file.text(name) == Some(spelling))
        }) {
            return false;
        }
        let mut local = module.to_vec();
        local.push(spelling.to_owned());
        !self.types.contains_key(&local)
            && !self.modules.contains_key(&local)
            && self
                .glob_target(
                    file,
                    position,
                    spelling,
                    Namespace::Type,
                    Some(binding.scope),
                    work,
                )
                .is_none()
            && !work.exhausted
    }

    fn extern_local_callee(
        &self,
        file: &File<'source>,
        position: u64,
        path: &[String],
        work: &mut Work,
    ) -> Option<Target<'source>> {
        self.lookup(file, position, path, Namespace::Value, work)
            .or_else(|| {
                self.lookup(file, position, path, Namespace::Type, work)
                    .filter(|target| target.symbol.kind == "struct")
            })
            .or_else(|| {
                let (member, receiver) = path.split_last()?;
                let receiver = self.lookup(file, position, receiver, Namespace::Type, work)?;
                self.inherent_method(file, position, receiver, member, work)
            })
    }

    /// The nearest admitted module binding owns the name, even when its target is unknown.
    fn extern_binding(
        &self,
        file: &File<'source>,
        bindings: impl Iterator<Item = &'source StaticBinding>,
        name: &str,
        module: &[String],
        position: u64,
        work: &mut Work,
    ) -> Binding<'source> {
        let mut selected: Option<&StaticBinding> = None;
        let mut ambiguous = false;
        for candidate in bindings {
            if !work.charge() {
                return Binding::Blocked;
            }
            if !candidate.scope.contains(position) || file.text(candidate.name) != Some(name) {
                continue;
            }
            let StaticBindingKind::RustExternCrate { crate_name } = candidate.kind else {
                continue;
            };
            if file.inactive(crate_name, work) {
                continue;
            }
            let Some((origin, inline)) = self.module_with_scope(file, crate_name.start, work)
            else {
                continue;
            };
            let physical = inline
                .or_else(|| crate::analyzer::namespace::rust_file_scope(file.document.source));
            // A crate-root alias can enter the extern prelude. A nested alias belongs to its module.
            if physical != Some(candidate.scope) || (origin != module && origin.len() != 1) {
                continue;
            }
            match selected {
                Some(current) if current.scope == candidate.scope => ambiguous = true,
                Some(current) if !current.scope.encloses(candidate.scope) => {}
                _ => {
                    selected = Some(candidate);
                    ambiguous = false;
                }
            }
        }
        if ambiguous || work.exhausted {
            Binding::Blocked
        } else {
            selected.map_or(Binding::Missing, Binding::Found)
        }
    }
}
