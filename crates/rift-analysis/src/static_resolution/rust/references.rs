//! Rust containment, field accesses, overrides, and type uses.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::{ByteRange, StaticBindingKind, StaticCall, StaticCallee, SyntaxSymbol};

use super::receivers::split_references;
use super::{
    Namespace, Namespaces, RECEIVER_DEPTH_MAX, inherent_member, is_type_binding, is_value_binding,
    path_segments,
};
use crate::static_resolution::{
    Binding, File, Index, StaticOccurrence, StaticResolution, Target, Work,
};

pub(super) enum ReceiverOwner<'source> {
    Target(Target<'source>),
    TypePath(ByteRange),
}

impl<'source> Index<'source> {
    /// Adds authored Rust local definitions and reads on the work left by other references.
    pub(in crate::static_resolution) fn rust_local_bindings(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let mut locals = BTreeMap::new();
        for symbol in file.document.facts.symbols() {
            if symbol.is_rust_receiver_binding()
                || symbol.is_rust_generic_parameter()
                || symbol.is_rust_library_root()
            {
                continue;
            }
            if !work.charge() {
                return;
            }
            if !symbol.is_rust_local_binding()
                || file
                    .document
                    .placement
                    .logical_identity(&symbol.qualified_name)
                    .is_none()
                || file.conditional(symbol.item_range, work)
            {
                continue;
            }
            let Some(range) = symbol.name_range else {
                continue;
            };
            let Some(parent) = file.parent_callable(symbol) else {
                continue;
            };
            let target = Target {
                file: file.document,
                symbol,
            };
            if let Some(occurrence) =
                self.rust_edge(file, range, EdgeKind::Contains, parent, target)
            {
                locals.insert(range, (target, parent.symbol.item_range));
                result.record(Some(occurrence));
            }
        }
        if locals.is_empty() {
            return;
        }
        let references = &file.facts.references()[file.primary_reference_count..];
        for reference in references
            .iter()
            .take_while(|reference| file.text(reference.target) != Some("self"))
        {
            if !work.charge() {
                return;
            }
            let read = reference.target;
            let Some(name) = file.text(read) else {
                continue;
            };
            if file.conditional(read, work) || file.blocked(read.start, work) {
                continue;
            }
            let Binding::Found(binding) = file.binding(name, read.start, is_value_binding, work)
            else {
                continue;
            };
            let Some(&(target, holder)) = locals.get(&binding.name) else {
                continue;
            };
            if file
                .enclosing_callable(read)
                .is_none_or(|caller| caller.symbol.item_range != holder)
            {
                continue;
            }
            if let Some(occurrence) = self.occurrence(file, read, read, ReferenceRole::Read, target)
            {
                result.record(Some(occurrence));
            }
        }
    }

    /// Adds receiver occurrences after the earlier declaration and reference classes.
    pub(in crate::static_resolution) fn resolve_rust_receivers(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        for file in self.files.values() {
            if work.exhausted {
                break;
            }
            if !file.rust_receivers.is_empty() {
                self.rust_receivers(file, work, result);
            }
        }
        if result.occurrences.len() != start {
            result.rust_receiver_start = Some(start);
        }
    }

    fn rust_receivers(&self, file: &File<'source>, work: &mut Work, result: &mut StaticResolution) {
        let mut receivers = BTreeMap::new();
        for &symbol in &file.rust_receivers {
            if !work.charge() {
                return;
            }
            let Some((name, method)) = symbol.name_range.zip(file.parent_callable(symbol)) else {
                continue;
            };
            let target = Target {
                file: file.document,
                symbol,
            };
            if let Some(edge) = self.rust_edge(file, name, EdgeKind::Contains, method, target) {
                result.record(Some(edge));
                receivers.insert(method.symbol.item_range, target);
            }
        }
        let references = &file.facts.references()[file.primary_reference_count..];
        for reference in references
            .iter()
            .rev()
            .take_while(|reference| file.text(reference.target) == Some("self"))
        {
            if !work.charge() {
                return;
            }
            let read = reference.target;
            let Some(method) = file.enclosing_callable(read) else {
                continue;
            };
            let Some(&target) = receivers.get(&method.symbol.item_range) else {
                continue;
            };
            if !method
                .symbol
                .body_range
                .is_some_and(|body| body.encloses(read))
                || file.conditional(read, work)
            {
                continue;
            }
            if let Some(occurrence) = self.occurrence(file, read, read, ReferenceRole::Read, target)
            {
                result.record(Some(occurrence));
            }
        }
    }

    /// Adds direct type annotation uses after the earlier occurrence classes.
    pub(in crate::static_resolution) fn resolve_rust_generics(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        for file in self.files.values() {
            if file.rust_generics.is_empty() {
                continue;
            }
            for reference in file.facts.rust_generic_references() {
                if !work.charge() {
                    break;
                }
                let Some(&symbol) = file.rust_generics.get(&reference.binding) else {
                    continue;
                };
                if file.text(reference.range) != Some(symbol.name.as_str())
                    || file.conditional(reference.range, work)
                {
                    continue;
                }
                let target = Target {
                    file: file.document,
                    symbol,
                };
                if let Some(occurrence) = self.occurrence(
                    file,
                    reference.range,
                    reference.range,
                    reference.role,
                    target,
                ) {
                    result.record(Some(occurrence));
                }
            }
            if work.exhausted {
                break;
            }
        }
        if result.occurrences.len() != start {
            result.rust_generic_start = Some(start);
        }
    }

    /// Retains containment from the selected Cargo file to its authored items.
    pub(in crate::static_resolution) fn resolve_rust_library_root(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        for file in self.files.values() {
            let Some(root) = file.rust_library_root else {
                continue;
            };
            let from = Target {
                file: file.document,
                symbol: root,
            };
            for symbol in file.document.facts.symbols() {
                if !work.charge() {
                    break;
                }
                if symbol.container.is_some()
                    || symbol.is_package_binding()
                    || symbol.kind == "impl_item"
                {
                    continue;
                }
                let Some(name) = symbol.name_range else {
                    continue;
                };
                let to = Target {
                    file: file.document,
                    symbol,
                };
                if let Some(edge) = self.rust_edge(file, name, EdgeKind::Contains, from, to) {
                    result.record(Some(edge));
                }
            }
        }
        if result.occurrences.len() != start {
            result.rust_library_root_start = Some(start);
        }
    }

    /// Resolves written crate tokens to the selected library declaration.
    pub(in crate::static_resolution) fn resolve_rust_crate_references(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
        select: impl Fn(&rift_syntax::StaticFacts) -> &[ByteRange],
    ) -> Option<usize> {
        let root = self.files.values().find_map(|file| {
            Some(Target {
                file: file.document,
                symbol: file.rust_library_root?,
            })
        })?;
        let crate_name = root.file.placement.module_path().first()?;
        let start = result.occurrences.len();
        for file in self.files.values() {
            if file.document.placement.module().is_none() {
                continue;
            }
            for &range in select(file.facts) {
                if !work.charge() {
                    break;
                }
                if file.text(range) != Some("crate")
                    || file.conditional(range, work)
                    || file.blocked(range.start, work)
                    || self
                        .rust
                        .module(file, range.start, work)
                        .and_then(|module| module.first())
                        != Some(crate_name)
                {
                    continue;
                }
                if let Some(occurrence) =
                    self.occurrence(file, range, range, ReferenceRole::Read, root)
                {
                    result.record(Some(occurrence));
                }
            }
            if work.exhausted {
                break;
            }
        }
        (result.occurrences.len() != start).then_some(start)
    }

    /// Reads the module and type tokens written before a qualified callee's final name.
    pub(in crate::static_resolution) fn rust_call_qualifiers(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for call in file.facts.calls() {
            if !work.charge() {
                return;
            }
            if call.callee == StaticCallee::Qualified
                && !file.conditional(call.range, work)
                && !file.blocked(call.range.start, work)
            {
                let _ = self.rust_qualified_callee(file, call, work, result);
            }
        }
    }

    fn rust_qualified_callee(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> Option<()> {
        let position = call.range.start;
        let text = file.text(call.callee_range)?;
        let parts = path_segments(text, work)?;
        let head = *parts.first()?;
        // Associated paths through a trait's Self need an implementing type, even when
        // the final name resolves to a trait item.
        if parts.len() < 2 || head == "Self" {
            return None;
        }
        // A body-local item can shadow a module declaration without itself being in the
        // module index. No path through that body-local item is admitted here.
        if !matches!(head, "crate" | "self" | "super")
            && Namespaces::local_named(file, position, head, work, qualifier_kind).is_some()
        {
            return None;
        }
        for (index, part) in parts.iter().take(parts.len().saturating_sub(1)).enumerate() {
            let prefix = &parts[..=index];
            // Resolving and copying the prefix takes work for every written segment.
            if !prefix.iter().all(|_| work.charge()) {
                return None;
            }
            let Some(target) = self
                .rust
                .resolved_path(file, position, prefix, work)
                .and_then(|path| {
                    self.rust
                        .lookup(file, position, &path, Namespace::Type, work)
                })
                .filter(|target| qualifier_kind(*target))
            else {
                continue;
            };
            let token = text.as_bytes().subslice_range(part.as_bytes())?;
            let range = ByteRange {
                start: call
                    .callee_range
                    .start
                    .checked_add(u64::try_from(token.start).ok()?)?,
                end: call
                    .callee_range
                    .start
                    .checked_add(u64::try_from(token.end).ok()?)?,
            };
            if let Some(occurrence) =
                self.occurrence(file, range, range, ReferenceRole::Read, target)
                && matches!(
                    occurrence.target,
                    rift_provider::NormalizedTarget::Symbol(_)
                )
            {
                result.occurrences.push(occurrence);
            }
        }
        Some(())
    }

    /// The local target of one captured Rust reference.
    fn rust_reference_occurrence(
        &self,
        file: &File<'source>,
        reference: &rift_syntax::StaticReference,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
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
        target.and_then(|target| {
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
        })
    }

    pub(in crate::static_resolution) fn rust_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for reference in &file.facts.references()[..file.primary_reference_count] {
            if !work.charge() {
                return;
            }
            if (reference.role == ReferenceRole::Type
                && file
                    .facts
                    .rust_generic_references()
                    .binary_search_by_key(&reference.target, |generic| generic.range)
                    .is_ok())
                || file.conditional(reference.target, work)
                || self
                    .rust
                    .module(file, reference.target.start, work)
                    .is_none()
            {
                continue;
            }
            let occurrence = self.rust_reference_occurrence(file, reference, work);
            if occurrence.is_none()
                && reference.role == ReferenceRole::Type
                && let Some(external) =
                    self.external_type(file, reference.target, work)
                        .and_then(|target| {
                            Self::external_occurrence(
                                file,
                                reference.target,
                                reference.target,
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
            if symbol.is_rust_local_binding() || symbol.is_rust_library_root() {
                continue;
            }
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
        occurrence.edge_from = self.rust_module_from(file, range, work);
        Some(occurrence)
    }

    /// The module whose `use` declaration stands at `range`. A reexport declares its name
    /// there as well, and that name imports nothing.
    pub(in crate::static_resolution) fn rust_module_from(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        let imported = file
            .facts
            .rust_uses()
            .iter()
            .find(|imported| work.charge() && imported.range == range)?;
        let scope = crate::analyzer::namespace::rust_file_scope(file.document.source)?;
        if imported.scope == scope {
            return file.document.placement.module().cloned();
        }
        let source = self.rust.import_source(file, range, work)?;
        let source = self.occurrence(file, range, range, ReferenceRole::Import, source)?;
        match source.target {
            rift_provider::NormalizedTarget::Symbol(from) => Some(from),
            rift_provider::NormalizedTarget::Contribution(_) => None,
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

fn qualifier_kind(target: Target<'_>) -> bool {
    target.symbol.kinds.contains(&SymbolKind::Module)
        || target.symbol.kinds.contains(&SymbolKind::Type)
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
                return self
                    .receiver_method_of(file, Some(receiver), receiver.start, (owner, member), work)
                    .or_else(|| {
                        self.returned_deref_member(file, receiver, owner, member, work, depth)
                    });
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
        self.bound_member(file, receiver.start, parameter, member, work)
    }

    /// The one member named `member` that the trait bounds of a type parameter declare;
    /// a bound that is not a known trait, or two bounds that declare it, give none.
    pub(super) fn bound_member(
        &self,
        file: &File<'source>,
        position: u64,
        parameter: &rift_syntax::StaticBinding,
        member: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
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
            let Some(target) = self.inherent_method(file, position, owner, member, work) else {
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
                StaticBindingKind::Shadow => {
                    self.closure_parameter_owner(file, binding, work, depth - 1)
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
                .map(|returned| ReceiverOwner::Target(returned.owner));
        }
        self.receiver_owner_at_depth(file, initializer, work, depth - 1)
    }

    /// The owner of a closure parameter passed to a local function: the callee declares
    /// the argument with a type parameter bounded by `Fn`, `FnMut` or `FnOnce`, the
    /// bound's parameter at the closure parameter's position is (a reference to) another
    /// type parameter of the callee, and the argument for the callee parameter declared
    /// with that type parameter has a known owner. A tuple pattern takes the element of a
    /// tuple argument. Any other shape leaves the parameter unresolved.
    fn closure_parameter_owner(
        &self,
        file: &File<'source>,
        binding: &rift_syntax::StaticBinding,
        work: &mut Work,
        depth: usize,
    ) -> Option<ReceiverOwner<'source>> {
        let closure = binding.scope;
        let (call_range, arguments) = file
            .facts
            .closure_calls()
            .iter()
            .find(|(_, arguments)| work.charge() && arguments.contains(&closure))?;
        let index = arguments.iter().position(|argument| *argument == closure)?;
        let offset = usize::try_from(binding.name.start.checked_sub(closure.start)?).ok()?;
        let (parameter, element) = closure_parameter_position(file.text(closure)?, offset)?;
        let call = self
            .initializers
            .get(&(file.document.path.as_str(), *call_range))
            .copied()
            .flatten()?;
        let member = matches!(call.callee, rift_syntax::StaticCallee::Member { .. });
        let callee = self.call_at_depth(file, call, work, depth)?;
        if !callee.symbol.kinds.contains(&SymbolKind::Callable) {
            return None;
        }
        let facts = callee.file.facts.static_facts()?;
        let text = |range: ByteRange| source_text(callee.file.source, range);
        let parameters = callee_parameters(callee, member)?;
        let declared = |parameter: &rift_syntax::StaticBinding| match parameter.kind {
            StaticBindingKind::RustReceiver { type_path } => text(type_path),
            _ => None,
        };
        let type_parameter = |name: &str| {
            facts.bindings().iter().find(|candidate| {
                candidate.kind == StaticBindingKind::RustTypeParameter
                    && candidate.scope.encloses(callee.symbol.item_range)
                    && text(candidate.name) == Some(name)
            })
        };
        let bound_parameter = type_parameter(declared(parameters.get(index)?)?)?;
        let mut selected = None;
        for &bound in self
            .bounds
            .get(&(callee.file.path.as_str(), bound_parameter.name))?
        {
            if !work.charge() {
                return None;
            }
            let Some(written) = closure_bound_parameter(text(bound)?, parameter) else {
                continue;
            };
            if selected.replace(written).is_some() {
                return None;
            }
        }
        let mut written = strip_reference(selected?);
        if let Some(element) = element
            && written.starts_with('(')
        {
            written = strip_reference(tuple_elements(written)?.get(element)?);
        }
        type_parameter(written)?;
        let position = parameters
            .iter()
            .position(|candidate| declared(candidate).map(strip_reference) == Some(written))?;
        let mut argument = *arguments.get(position)?;
        if let Some(element) = element
            && !selected?.contains('(')
        {
            argument = tuple_element_range(file, argument, element)?;
        }
        let argument = dereferenced(file, super::reference_operand(file, argument)?)?;
        self.receiver_owner_at_depth(file, argument, work, depth)
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

    /// The declaration that contains `symbol`. The header of an implementation is a facet of
    /// the type it names, so a module holding it contains it only when it declares the type.
    fn container(
        &self,
        file: &File<'source>,
        symbol: &'source SyntaxSymbol,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let parent = self.parent(file, symbol, work)?;
        if symbol.kind != "impl_item" {
            return Some(parent);
        }
        let path = Target {
            file: file.document,
            symbol,
        }
        .admitted_path()?;
        let holder = parent.admitted_path()?;
        let declares = path
            .split_last()
            .is_some_and(|(_, owner)| owner == holder.as_slice());
        // The written path of a foreign type keeps its header below the module of the block.
        let foreign = path.len() > holder.len()
            && path.starts_with(&holder)
            && !self.types.contains_key(&path);
        (declares || foreign).then_some(parent)
    }

    fn parent(
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

/// The closure parameter at byte `offset` of a closure's text: its position, and the element
/// position when the parameter is a tuple pattern. Annotated or nested patterns give none.
fn closure_parameter_position(closure: &str, offset: usize) -> Option<(usize, Option<usize>)> {
    let open = closure.find('|')?;
    let close = open
        .checked_add(1)?
        .checked_add(closure.get(open + 1..)?.find('|')?)?;
    if offset <= open || offset >= close {
        return None;
    }
    let mut start = open + 1;
    for (index, parameter) in split_top_level(closure.get(open + 1..close)?)?
        .into_iter()
        .enumerate()
    {
        let end = start + parameter.len();
        if (start..end).contains(&offset) {
            let trimmed = parameter.trim();
            if trimmed.contains(':') {
                return None;
            }
            let Some(inner) = trimmed
                .strip_prefix('(')
                .and_then(|rest| rest.strip_suffix(')'))
            else {
                return Some((index, None));
            };
            let inner_start = start + parameter.find('(')? + 1;
            let mut element_start = inner_start;
            for (element, written) in split_top_level(inner)?.into_iter().enumerate() {
                let element_end = element_start + written.len();
                if (element_start..element_end).contains(&offset) {
                    return (!written.contains(['(', '[', '{'])).then_some((index, Some(element)));
                }
                element_start = element_end + 1;
            }
            return None;
        }
        start = end + 1;
    }
    None
}

/// Comma-separated parts at nesting depth zero; unbalanced text gives none.
pub(super) fn split_top_level(text: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' | b'<' => depth = depth.checked_add(1)?,
            b'>' if index > 0 && bytes[index - 1] == b'-' => {}
            b')' | b']' | b'}' | b'>' => depth = depth.checked_sub(1)?,
            b',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    if !text[start..].trim().is_empty() {
        parts.push(&text[start..]);
    }
    Some(parts)
}

/// The parameter type at `position` of an `Fn(..)`, `FnMut(..)` or `FnOnce(..)` bound.
fn closure_bound_parameter(bound: &str, position: usize) -> Option<&str> {
    let bound = bound.trim();
    let rest = ["FnOnce", "FnMut", "Fn"]
        .iter()
        .find_map(|name| bound.strip_prefix(name))?
        .trim_start()
        .strip_prefix('(')?;
    let mut depth = 1_usize;
    let mut close = None;
    for (index, byte) in rest.bytes().enumerate() {
        match byte {
            b'(' => depth = depth.checked_add(1)?,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    split_top_level(&rest[..close?])?
        .get(position)
        .map(|written| written.trim())
}

/// A written type without leading `&`, `&mut` and lifetime references.
fn strip_reference(written: &str) -> &str {
    split_references(written).1
}

/// The elements of a parenthesized tuple type or expression with at least one comma.
fn tuple_elements(written: &str) -> Option<Vec<&str>> {
    let inner = written.trim().strip_prefix('(')?.strip_suffix(')')?;
    let elements = split_top_level(inner)?;
    (elements.len() > 1 || inner.trim_end().ends_with(','))
        .then(|| elements.into_iter().map(str::trim).collect())
}

/// The range of element `element` of the tuple expression at `range`.
fn tuple_element_range(file: &File<'_>, range: ByteRange, element: usize) -> Option<ByteRange> {
    let text = file.text(range)?;
    let inner = text.trim_end().strip_prefix('(')?.strip_suffix(')')?;
    let mut start = 1;
    for (index, written) in split_top_level(inner)?.into_iter().enumerate() {
        if index == element {
            let leading = written.len() - written.trim_start().len();
            let begin = range
                .start
                .checked_add(u64::try_from(start + leading).ok()?)?;
            return Some(ByteRange {
                start: begin,
                end: begin.checked_add(u64::try_from(written.trim().len()).ok()?)?,
            });
        }
        start += written.len() + 1;
    }
    None
}

pub(super) fn source_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

/// The declared parameters of a callee in order, without `self` for a method call.
pub(super) fn callee_parameters(
    callee: Target<'_>,
    member: bool,
) -> Option<Vec<&rift_syntax::StaticBinding>> {
    let facts = callee.file.facts.static_facts()?;
    let body = callee.symbol.body_range?;
    let mut parameters = facts
        .bindings()
        .iter()
        .filter(|candidate| {
            candidate.scope == callee.symbol.item_range
                && candidate.name.start < body.start
                && matches!(
                    candidate.kind,
                    StaticBindingKind::RustReceiver { .. }
                        | StaticBindingKind::Shadow
                        | StaticBindingKind::RustValue { .. }
                )
        })
        .collect::<Vec<_>>();
    parameters.sort_by_key(|candidate| candidate.name.start);
    if member {
        parameters
            .retain(|candidate| source_text(callee.file.source, candidate.name) != Some("self"));
    }
    Some(parameters)
}

/// The operand of a written `*value`, else the range itself.
fn dereferenced(file: &File<'_>, range: ByteRange) -> Option<ByteRange> {
    let Some(rest) = file.text(range)?.strip_prefix('*') else {
        return Some(range);
    };
    Some(ByteRange {
        start: range
            .end
            .checked_sub(u64::try_from(rest.trim_start().len()).ok()?)?,
        end: range.end,
    })
}
