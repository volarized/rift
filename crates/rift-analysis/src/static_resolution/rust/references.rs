//! Rust containment, field accesses, overrides, and type uses.

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::{ByteRange, StaticBindingKind, SyntaxSymbol};

use super::{Namespaces, inherent_member, is_value_binding};
use crate::static_resolution::{
    Binding, File, Index, StaticOccurrence, StaticResolution, Target, Work,
};

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn rust_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            let target = if reference.role == ReferenceRole::Type {
                self.rust.edge_type_target(file, reference.range, work)
            } else {
                self.rust
                    .access(file, reference.range, work)
                    .filter(|target| {
                        reference.role != ReferenceRole::Write
                            || target.symbol.kind == "field_declaration"
                    })
            };
            let occurrence = target.and_then(|target| {
                let mut occurrence = self.occurrence(
                    file,
                    reference.range,
                    reference.range,
                    reference.role,
                    target,
                )?;
                if reference.role != ReferenceRole::Type
                    && let Some(caller) = file.enclosing_callable(reference.range)
                {
                    let source = self.occurrence(
                        file,
                        reference.range,
                        reference.range,
                        reference.role,
                        caller,
                    )?;
                    if let rift_provider::NormalizedTarget::Symbol(from) = source.target {
                        occurrence.edge_from = Some(from);
                    }
                }
                Some(occurrence)
            });
            if occurrence.is_none()
                && reference.role == ReferenceRole::Type
                && let Some(external) =
                    self.external_type(file, reference.range, work)
                        .and_then(|target| {
                            Self::external_occurrence(
                                file,
                                reference.range,
                                reference.range,
                                reference.role,
                                target,
                            )
                        })
            {
                result.external.push(external);
                continue;
            }
            result.record(occurrence);
        }
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return;
            }
            let target = Target {
                file: file.document,
                symbol,
            };
            let range = symbol.name_range.unwrap_or(symbol.item_range);
            if let Some(parent) = self.rust.container(file, symbol, work) {
                result.record(self.rust_edge(file, range, EdgeKind::Contains, parent, target));
            }
            if symbol.kinds.contains(&SymbolKind::Callable)
                && let Some(base) = self.rust.trait_member(file, symbol, work)
            {
                result.record(self.rust_edge(file, range, EdgeKind::Overrides, target, base));
            }
        }
        for imported in file
            .facts
            .rust_uses()
            .iter()
            .filter(|imported| imported.glob)
        {
            if !work.charge() {
                return;
            }
            let occurrence = self
                .rust
                .import(file, imported.range, work)
                .and_then(|target| {
                    self.occurrence(
                        file,
                        imported.range,
                        imported.range,
                        ReferenceRole::Import,
                        target,
                    )
                });
            result.record(occurrence);
        }
    }

    fn rust_edge(
        &self,
        file: &File<'source>,
        range: ByteRange,
        kind: EdgeKind,
        from: Target<'source>,
        to: Target<'source>,
    ) -> Option<StaticOccurrence> {
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Read, to)?;
        let source = self.occurrence(file, range, range, ReferenceRole::Read, from)?;
        let rift_provider::NormalizedTarget::Symbol(from) = source.target else {
            return None;
        };
        occurrence.edge_kind = Some(kind);
        occurrence.edge_from = Some(from);
        Some(occurrence)
    }
}

impl<'source> Namespaces<'source> {
    fn access(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (receiver, member) = file.text(range)?.rsplit_once('.')?;
        let receiver = ByteRange {
            start: range.start,
            end: range
                .start
                .checked_add(u64::try_from(receiver.len()).ok()?)?,
        };
        let owner = self.receiver_target(file, receiver, work)?;
        let mut path = owner.admitted_path()?;
        path.push(member.trim().to_owned());
        self.members
            .get(&path)
            .copied()
            .flatten()
            .filter(|target| target.symbol.kind == "field_declaration")
            .or_else(|| self.inherent_method(file, range.start, owner, member.trim(), work))
    }

    pub(super) fn receiver_target(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut parts = file.text(range)?.split('.').map(str::trim);
        let receiver = parts.next()?;
        let Binding::Found(binding) = file.binding(receiver, range.start, is_value_binding, work)
        else {
            return None;
        };
        let StaticBindingKind::RustReceiver { type_path } = binding.kind else {
            return None;
        };
        let mut owner = self.edge_type_target(file, type_path, work)?;
        for member in parts {
            if !work.charge() {
                return None;
            }
            let mut path = owner.admitted_path()?;
            path.push(member.to_owned());
            owner = *self.field_types.get(&path)?;
        }
        Some(owner)
    }

    fn container(
        &self,
        file: &File<'source>,
        symbol: &SyntaxSymbol,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if let Some(implementation) = inherent_member(file, symbol, work) {
            return self.edge_type_target(file, implementation.type_path, work);
        }
        if let Some(container) = symbol.container.as_deref() {
            if let Some(target) = file
                .parent_callable(symbol)
                .or_else(|| file.qualified_target(container))
            {
                return Some(target);
            }
            let implementation = file
                .facts
                .inherent_impls()
                .iter()
                .find(|implementation| implementation.range.encloses(symbol.item_range))?;
            return self.edge_type_target(file, implementation.type_path, work);
        }
        let path = self.module(file, symbol.item_range.start, work)?;
        self.types.get(path).copied().flatten().filter(|target| {
            target.symbol.item_range != symbol.item_range || target.file.path != file.document.path
        })
    }

    fn trait_member(
        &self,
        file: &File<'source>,
        symbol: &SyntaxSymbol,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let implementation = inherent_member(file, symbol, work)?;
        let edge = file
            .facts
            .heritage_edges()
            .iter()
            .find(|edge| edge.from == implementation.type_path)?;
        let owner = self.edge_type_target(file, edge.target, work)?;
        let mut path = owner.admitted_path()?;
        path.push(symbol.name.clone());
        self.members.get(&path).copied().flatten()
    }
}
