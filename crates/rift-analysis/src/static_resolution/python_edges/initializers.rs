//! Python initializer keywords whose call and defining field both have authored evidence.

use std::collections::BTreeMap;

use rift_core::{DeclarationBinding, ReferenceRole, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::SymbolKind;
use rift_provider::NormalizedTarget;
use rift_syntax::{ByteRange, StaticCall, StaticPythonInitializer};

use super::super::{File, Index, StaticResolution, Target, Work};

/// Primary Call targets retained for the later keyword pass.
#[derive(Default)]
pub(in crate::static_resolution) struct InitializerCalls(
    BTreeMap<ByteRange, (SymbolId, DeclarationBinding)>,
);

impl InitializerCalls {
    pub(in crate::static_resolution) fn capture(
        file: &File<'_>,
        result: &StaticResolution,
    ) -> Option<Self> {
        if file.facts.python_keyword_arguments().is_empty() {
            return None;
        }
        let calls: BTreeMap<_, _> = result
            .occurrences
            .iter()
            .filter_map(|call| {
                if call.role != ReferenceRole::Call {
                    return None;
                }
                let NormalizedTarget::Symbol(target) = &call.target else {
                    return None;
                };
                let range = call.binding.range();
                let binding = call.target_binding.as_ref()?;
                Some((
                    ByteRange {
                        start: range.start(),
                        end: range.end(),
                    },
                    (target.clone(), binding.clone()),
                ))
            })
            .collect();
        (!calls.is_empty()).then_some(Self(calls))
    }
}

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn python_initializer_keywords(
        &self,
        file: &File<'source>,
        observed: &InitializerCalls,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let mut calls = BTreeMap::new();
        for call in file.facts.calls() {
            if !work.charge() {
                return;
            }
            if observed.0.contains_key(&call.range) {
                calls.insert(call.range, call);
            }
        }
        for &(span, name) in file.facts.python_keyword_arguments() {
            if !work.charge() {
                return;
            }
            if let Some(call) = calls.get(&span)
                && let Some(field) =
                    self.python_initializer_keyword(file, observed, call, name, work)
            {
                result.record(self.python_access(file, name, ReferenceRole::Read, field));
            }
        }
    }

    fn python_initializer_keyword(
        &self,
        file: &File<'source>,
        observed: &InitializerCalls,
        call: &StaticCall,
        name: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (symbol, declaration) = observed.0.get(&call.range)?;
        let class = self.call_target(file, call, work)?;
        if !class.symbol.kinds.contains(&SymbolKind::Constructible) {
            return None;
        }
        let selected = self.occurrence(
            file,
            call.range,
            call.callee_range,
            ReferenceRole::Call,
            class,
        )?;
        if selected.target_binding.as_ref() != Some(declaration)
            || selected.target != NormalizedTarget::Symbol(symbol.clone())
        {
            return None;
        }
        self.python_attrs_field(class, file.text(name)?, work)
    }

    fn python_attrs_field(
        &self,
        mut class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut selected = None;
        while work.charge() {
            let held = self.files.get(class.file.path.as_str())?;
            let initializer = self.python_attrs_initializer(held, class, work)?;
            if selected.is_none() {
                for field in &initializer.fields {
                    if !work.charge() {
                        return None;
                    }
                    if held.text(field.name) != Some(name) {
                        continue;
                    }
                    if !self.python_initializer_annotation(held, field.annotation, work) {
                        return None;
                    }
                    let member = self.python_own_member(class, name, work)?;
                    if member.symbol.name_range != Some(field.name) {
                        return None;
                    }
                    selected = Some(member);
                    break;
                }
                if selected.is_none() && self.python_own_member(class, name, work).is_some() {
                    return None;
                }
            }
            let Some(base) = initializer.base else {
                return selected;
            };
            class = self.edge_python_base(held, base, work)?;
        }
        None
    }

    fn python_attrs_initializer<'held>(
        &self,
        file: &'held File<'source>,
        class: Target<'source>,
        work: &mut Work,
    ) -> Option<&'held StaticPythonInitializer> {
        for initializer in file.facts.python_initializers() {
            if !work.charge() {
                return None;
            }
            if initializer.class != class.symbol.item_range {
                continue;
            }
            let qualified = self.python_library_name(file, initializer.decorator, work)?;
            let (module, generator) = qualified.rsplit_once('.')?;
            if matches!(module, "attrs" | "attr")
                && matches!(generator, "define" | "frozen" | "mutable")
                && self.python_writes.get(generator).is_none_or(Vec::is_empty)
            {
                return Some(initializer);
            }
            return None;
        }
        None
    }

    fn python_initializer_annotation(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> bool {
        let Some(written) = file.text(range) else {
            return false;
        };
        if !matches!(
            written,
            "bool"
                | "bytes"
                | "dict"
                | "float"
                | "int"
                | "list"
                | "object"
                | "set"
                | "str"
                | "tuple"
                | "type"
        ) {
            return false;
        }
        self.external_type(file, range, work)
            .and_then(|symbol| SymbolIdentity::parse(symbol.as_str()).ok())
            .is_some_and(|identity| {
                matches!(identity.owner(), SymbolOwner::Runtime { runtime } if runtime == "cpython")
                    && identity
                        .qualified_path()
                        .iter()
                        .map(String::as_str)
                        .eq(["builtins", written])
            })
    }
}
