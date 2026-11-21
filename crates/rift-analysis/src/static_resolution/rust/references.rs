//! Rust containment, field accesses, overrides, and type uses.

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::{ByteRange, StaticBindingKind, SyntaxSymbol};

use super::{Namespaces, RECEIVER_DEPTH_MAX, inherent_member, is_type_binding, is_value_binding};
use crate::static_resolution::{
    Binding, File, Index, StaticOccurrence, StaticResolution, Target, Work,
};

pub(super) enum ReceiverOwner<'source> {
    Target(Target<'source>),
    TypePath(ByteRange),
}

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
            if file.conditional(reference.target, work)
                || self
                    .rust
                    .module(file, reference.target.start, work)
                    .is_none()
            {
                continue;
            }
            let target = if reference.role == ReferenceRole::Type {
                self.rust.edge_type_target(file, reference.target, work)
            } else {
                self.rust
                    .access(file, reference.target, work)
                    .filter(|target| {
                        reference.role != ReferenceRole::Write
                            || target.symbol.kind == "field_declaration"
                    })
            };
            let occurrence = target.and_then(|target| {
                let mut occurrence = self.occurrence(
                    file,
                    reference.target,
                    reference.target,
                    reference.role,
                    target,
                )?;
                if reference.role != ReferenceRole::Type
                    && let Some(caller) = file.enclosing_callable(reference.target)
                {
                    let source = self.occurrence(
                        file,
                        reference.target,
                        reference.target,
                        reference.role,
                        caller,
                    )?;
                    if let rift_provider::NormalizedTarget::Symbol(from) = source.target {
                        occurrence.edge_from = Some(from);
                    }
                }
                Some(occurrence)
            });
            result.record(occurrence);
        }
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return;
            }
            if file.conditional(symbol.item_range, work)
                || self
                    .rust
                    .module(file, symbol.item_range.start, work)
                    .is_none()
            {
                continue;
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
        for imported in file.facts.rust_uses() {
            if !work.charge() {
                return;
            }
            let target = if imported.glob {
                self.rust.import(file, imported.range, work)
            } else {
                self.rust.written_import(file, imported.range, work)
            };
            let occurrence = target
                .and_then(|target| self.rust_import_occurrence(file, imported.range, target, work));
            result.record(occurrence);
        }
    }

    pub(in crate::static_resolution) fn rust_import_occurrence(
        &self,
        file: &File<'source>,
        range: ByteRange,
        target: Target<'source>,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Import, target)?;
        if let Some(source) = self.rust.import_source(file, range, work) {
            let source = self.occurrence(file, range, range, ReferenceRole::Import, source)?;
            if let rift_provider::NormalizedTarget::Symbol(from) = source.target {
                occurrence.edge_from = Some(from);
            }
        }
        Some(occurrence)
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
        if let Some(call) = self.member_calls.get(&(file.document.path.as_str(), range)) {
            let rift_syntax::StaticCallee::Member { receiver, member } = (*call)?.callee else {
                return None;
            };
            return self.receiver_method(file, receiver, file.text(member)?, work);
        }
        let (receiver, member) = file.text(range)?.rsplit_once('.')?;
        let receiver = ByteRange {
            start: range.start,
            end: range
                .start
                .checked_add(u64::try_from(receiver.len()).ok()?)?,
        };
        self.receiver_target(file, receiver, work)
            .and_then(|owner| {
                let mut path = self.field_owner(owner).admitted_path()?;
                path.push(member.trim().to_owned());
                self.fields.get(&path).copied().flatten().filter(|target| {
                    target.symbol.kind == "field_declaration"
                        && self.available(*target)
                        && self.declaration_visible(file, range.start, *target, work)
                })
            })
    }

    pub(super) fn receiver_method(
        &self,
        file: &File<'source>,
        receiver: ByteRange,
        member: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        self.receiver_method_at_depth(file, receiver, member, work, RECEIVER_DEPTH_MAX)
    }

    pub(super) fn receiver_method_at_depth(
        &self,
        file: &File<'source>,
        receiver: ByteRange,
        member: &str,
        work: &mut Work,
        depth: usize,
    ) -> Option<Target<'source>> {
        if depth == 0 {
            return None;
        }
        let type_path = match self.receiver_owner_at_depth(file, receiver, work, depth)? {
            ReceiverOwner::Target(owner) => {
                return self.receiver_method_of(file, receiver.start, owner, member, work);
            }
            ReceiverOwner::TypePath(type_path) => type_path,
        };
        let name = file.text(type_path)?;
        let Binding::Found(parameter) = file.binding(name, type_path.start, is_type_binding, work)
        else {
            return None;
        };
        if parameter.kind != StaticBindingKind::RustTypeParameter {
            return None;
        }
        let mut selected: Option<Target<'source>> = None;
        let bounds = self
            .bounds
            .get(&(file.document.path.as_str(), parameter.name))?;
        for &bound in bounds {
            if !work.charge() {
                return None;
            }
            let owner = self
                .edge_type_target(file, bound, work)
                .filter(|owner| owner.symbol.kind == "trait")?;
            let Some(target) = self.inherent_method(file, receiver.start, owner, member, work)
            else {
                continue;
            };
            if selected.is_some_and(|previous| previous.symbol != target.symbol) {
                return None;
            }
            selected = Some(target);
        }
        selected
    }

    pub(super) fn receiver_target(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match self.receiver_owner_at_depth(file, range, work, RECEIVER_DEPTH_MAX)? {
            ReceiverOwner::Target(owner) => Some(owner),
            ReceiverOwner::TypePath(_) => None,
        }
    }

    pub(super) fn receiver_owner_at_depth(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
        depth: usize,
    ) -> Option<ReceiverOwner<'source>> {
        if depth == 0 || !work.charge() {
            return None;
        }
        if self
            .initializers
            .contains_key(&(file.document.path.as_str(), range))
        {
            return self.value_owner(file, range, work, depth - 1);
        }
        if let Some((receiver, member)) = file.text(range)?.rsplit_once('.') {
            let receiver = ByteRange {
                start: range.start,
                end: range
                    .start
                    .checked_add(u64::try_from(receiver.len()).ok()?)?,
            };
            let ReceiverOwner::Target(target) =
                self.receiver_owner_at_depth(file, receiver, work, depth - 1)?
            else {
                return None;
            };
            let mut path = self.field_owner(target).admitted_path()?;
            path.push(member.trim().to_owned());
            self.fields.get(&path).copied().flatten().filter(|target| {
                self.available(*target)
                    && self.declaration_visible(file, range.start, *target, work)
            })?;
            return self
                .field_types
                .get(&path)
                .copied()
                .map(ReceiverOwner::Target);
        }
        let receiver = file.text(range)?.trim();
        match file.binding(receiver, range.start, is_value_binding, work) {
            Binding::Found(binding) => match binding.kind {
                StaticBindingKind::RustReceiver { type_path } => Some(
                    self.edge_type_target(file, type_path, work)
                        .map_or(ReceiverOwner::TypePath(type_path), ReceiverOwner::Target),
                ),
                StaticBindingKind::RustValue { initializer } => {
                    self.value_owner(file, initializer, work, depth - 1)
                }
                _ => None,
            },
            Binding::Missing if receiver == "Self" => self
                .edge_type_target(file, range, work)
                .filter(|owner| owner.symbol.kind == "struct" && owner.symbol.body_range.is_none())
                .map(ReceiverOwner::Target),
            _ => None,
        }
    }

    fn value_owner(
        &self,
        file: &File<'source>,
        initializer: ByteRange,
        work: &mut Work,
        depth: usize,
    ) -> Option<ReceiverOwner<'source>> {
        if depth == 0 {
            return None;
        }
        if let Some(call) = self
            .initializers
            .get(&(file.document.path.as_str(), initializer))
        {
            let call = (*call)?;
            if let Some(owner) = self.cloned_owner(file, call, work, depth - 1) {
                return Some(ReceiverOwner::Target(owner));
            }
            let callable = self
                .call_at_depth(file, call, work, depth - 1)
                .or_else(|| {
                    if call.callee != rift_syntax::StaticCallee::Unsupported {
                        return None;
                    }
                    self.function_field(file, call.callee_range, work, depth - 1)
                })?;
            // A struct expression or tuple struct constructor makes a value of that struct.
            if callable.symbol.kind == "struct" {
                return Some(ReceiverOwner::Target(callable));
            }
            return self
                .return_owners
                .get(&(callable.file.path.as_str(), callable.symbol.name_range?))
                .copied()
                .map(ReceiverOwner::Target);
        }
        self.receiver_owner_at_depth(file, initializer, work, depth - 1)
    }

    /// `value.clone()` without a local `clone` for the receiver's type: the standard
    /// `Clone::clone` returns the receiver's type (`T` for a `T: Clone` receiver, else the
    /// reference itself), whose members are the same.
    fn cloned_owner(
        &self,
        file: &File<'source>,
        call: &rift_syntax::StaticCall,
        work: &mut Work,
        depth: usize,
    ) -> Option<Target<'source>> {
        let rift_syntax::StaticCallee::Member { receiver, member } = call.callee else {
            return None;
        };
        let written = file.text(call.range)?;
        if file.text(member)? != "clone"
            || !written
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
                .ends_with(".clone()")
        {
            return None;
        }
        let ReceiverOwner::Target(owner) =
            self.receiver_owner_at_depth(file, receiver, work, depth)?
        else {
            return None;
        };
        let mut path = self.field_owner(owner).admitted_path()?;
        path.push("clone".to_owned());
        (!self.methods.contains_key(&path) && !self.members.contains_key(&path)).then_some(owner)
    }

    fn function_field(
        &self,
        file: &File<'source>,
        mut range: ByteRange,
        work: &mut Work,
        depth: usize,
    ) -> Option<Target<'source>> {
        if depth == 0 {
            return None;
        }
        // Only a captured field expression can supply a function-pointer return.
        // Parenthesized invocation syntax keeps the field's original inner range.
        for _ in 0..depth {
            let text = file.text(range)?;
            if !text.starts_with('(') || !text.ends_with(')') {
                break;
            }
            range.start = range.start.checked_add(1)?;
            range.end = range.end.checked_sub(1)?;
            let text = file.text(range)?;
            range.start = range
                .start
                .checked_add(u64::try_from(text.len() - text.trim_start().len()).ok()?)?;
            range.end = range
                .end
                .checked_sub(u64::try_from(text.len() - text.trim_end().len()).ok()?)?;
        }
        if !file.facts.references().iter().any(|reference| {
            work.charge() && reference.target == range && reference.role == ReferenceRole::Read
        }) {
            return None;
        }
        let (receiver, member) = file.text(range)?.rsplit_once('.')?;
        let receiver = ByteRange {
            start: range.start,
            end: range
                .start
                .checked_add(u64::try_from(receiver.len()).ok()?)?,
        };
        let ReceiverOwner::Target(owner) =
            self.receiver_owner_at_depth(file, receiver, work, depth - 1)?
        else {
            return None;
        };
        let mut path = self.field_owner(owner).admitted_path()?;
        path.push(member.trim().to_owned());
        self.fields.get(&path).copied().flatten().filter(|target| {
            self.available(*target) && self.declaration_visible(file, range.start, *target, work)
        })
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
