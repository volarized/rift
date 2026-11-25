//! Python containment, member accesses, overrides, and annotation uses.

use std::collections::BTreeMap;

use rift_core::{ReferenceRole, SymbolId};
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_provider::NormalizedTarget;
use rift_syntax::{ByteRange, StaticReference};

use super::super::{File, Index, StaticOccurrence, StaticResolution, Target, Work};
use super::Value;

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn python_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let callees = established_callees(result);
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            // A member access spells its receiver. A name alone states no member.
            if reference.role != ReferenceRole::Type
                && file
                    .text(reference.target)
                    .is_some_and(|text| !text.contains('.'))
            {
                continue;
            }
            let occurrence = self
                .edge_python_name(file, reference.target, work)
                .or_else(|| {
                    (reference.role == ReferenceRole::Write)
                        .then(|| self.python_written_module_name(file, reference.target, work))
                        .flatten()
                })
                .and_then(|target| {
                    let mut occurrence = self.occurrence(
                        file,
                        reference.target,
                        reference.target,
                        reference.role,
                        target,
                    )?;
                    // An annotation belongs to its declaration. Accesses belong to the enclosing callable.
                    if reference.role != ReferenceRole::Type
                        && let Some(caller) = file.enclosing_callable(reference.target)
                    {
                        occurrence.edge_from = self.python_identity(file, reference.target, caller);
                    }
                    Some(occurrence)
                });
            if !names_callee(&callees, reference, occurrence.as_ref()) {
                result.record(occurrence);
            }
        }
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return;
            }
            if symbol.qualified_name == "." {
                continue;
            }
            let Some(range) = symbol
                .name_range
                .or(Some(symbol.item_range))
                .filter(|range| range.start < range.end)
            else {
                continue;
            };
            let target = Target {
                file: file.document,
                symbol,
            };
            let parent = file.qualified_target(symbol.container.as_deref().unwrap_or("."));
            if let Some(parent) = parent {
                result.record(self.python_edge(file, range, EdgeKind::Contains, parent, target));
                if symbol.kinds.contains(&SymbolKind::Callable)
                    && parent.symbol.kinds.contains(&SymbolKind::Constructible)
                    && let Some(base) = self.python_declared_base_member(parent, &symbol.name, work)
                    && base.symbol.kinds.contains(&SymbolKind::Callable)
                {
                    result.record(self.python_edge(file, range, EdgeKind::Overrides, target, base));
                }
            }
        }
    }

    /// The declaration a write to `module.name` names: the module's own declaration of `name`,
    /// even when other writes replace its value at runtime.
    fn python_written_module_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (receiver, member) = file.text(range)?.rsplit_once('.')?;
        let receiver = ByteRange {
            start: range.start,
            end: range.start + u64::try_from(receiver.len()).ok()?,
        };
        let Value::Module(name) =
            work.python_lookup(|work| self.python_name(file, receiver, work))?
        else {
            return None;
        };
        let module = self.files.get(self.edge_python.path(&name)?)?;
        let mut declaration: Option<Target<'source>> = None;
        for binding in module.by_name.get(member.trim())? {
            if !work.charge() {
                return None;
            }
            if !module.python_module_binding(binding) {
                continue;
            }
            let target = module.target_at(binding.name)?;
            if declaration
                .is_some_and(|known| known.symbol.qualified_name != target.symbol.qualified_name)
            {
                return None;
            }
            declaration.get_or_insert(target);
        }
        declaration
    }

    pub(in crate::static_resolution) fn python_identity(
        &self,
        file: &File<'source>,
        range: ByteRange,
        target: Target<'source>,
    ) -> Option<SymbolId> {
        let occurrence = self.occurrence(file, range, range, ReferenceRole::Read, target)?;
        match occurrence.target {
            NormalizedTarget::Symbol(symbol) => Some(symbol),
            NormalizedTarget::Contribution(_) => None,
        }
    }

    fn python_edge(
        &self,
        file: &File<'source>,
        range: ByteRange,
        kind: EdgeKind,
        from: Target<'source>,
        to: Target<'source>,
    ) -> Option<StaticOccurrence> {
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Read, to)?;
        occurrence.edge_kind = Some(kind);
        occurrence.edge_from = Some(self.python_identity(file, range, from)?);
        Some(occurrence)
    }
}

/// The callees whose calls in one file have a target, with the declaration each call names
/// in the selection.
fn established_callees(result: &StaticResolution) -> BTreeMap<ByteRange, Option<SymbolId>> {
    let external = result.external.iter();
    let mut callees = external
        .filter(|call| call.role == ReferenceRole::Call)
        .map(|call| (call.callee_range, None))
        .collect::<BTreeMap<_, _>>();
    for call in &result.occurrences {
        if call.role == ReferenceRole::Call
            && let NormalizedTarget::Symbol(called) = &call.target
        {
            callees.insert(call.callee_range, Some(called.clone()));
        }
    }
    callees
}

/// Whether the read of a callee names what its call names, or names nothing beside it. A
/// callee that is read through another name, such as an attribute that holds the called
/// class, is a fact of its own.
fn names_callee(
    callees: &BTreeMap<ByteRange, Option<SymbolId>>,
    reference: &StaticReference,
    occurrence: Option<&StaticOccurrence>,
) -> bool {
    let Some(called) = callees.get(&reference.target) else {
        return false;
    };
    let read = occurrence.map(|occurrence| &occurrence.target);
    reference.role == ReferenceRole::Read
        && match (called, read) {
            (Some(called), Some(NormalizedTarget::Symbol(read))) => called == read,
            (_, read) => read.is_none(),
        }
}
