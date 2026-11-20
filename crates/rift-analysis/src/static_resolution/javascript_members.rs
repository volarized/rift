//! Resolves explicit ECMAScript receivers and statically named base members.

use std::collections::BTreeSet;

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};

use super::javascript::MemberMode;
use super::{File, Index, Target, Work};

/// Maximum aliases followed while resolving an explicit receiver type.
const DECLARED_TYPE_DEPTH_MAX: u32 = 32;

impl<'source> Index<'source> {
    pub(super) fn javascript_object_member(
        &self,
        mut owner: Target<'source>,
        member: &str,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        for member in member.split('.').map(str::trim) {
            if !work.charge() {
                return None;
            }
            let source = self.files.get(owner.file.path.as_str())?;
            if role != ReferenceRole::Type
                && let Some(annotation) = owner
                    .symbol
                    .name_range
                    .and_then(|name| source.declared_types.get(&name))
                    .copied()
                    .flatten()
            {
                let declared = self.javascript_declared_type(source, annotation, work)?;
                owner = self.javascript_class_member(
                    declared,
                    member,
                    MemberMode::Instance,
                    role,
                    work,
                )?;
                continue;
            }
            let file = self.files.get(owner.file.path.as_str())?;
            let declared =
                file.qualified_target(&format!("{}.{}", owner.symbol.qualified_name, member));
            if let Some(target) = declared.filter(|target| {
                target.symbol.syntax_element_kind == Some("assignment_expression")
                    && target.symbol.kind == "method"
                    && (!matches!(owner.symbol.kind, "function" | "class")
                        || target.symbol.kinds.contains(&SymbolKind::Static))
            }) {
                if role == ReferenceRole::Call
                    && !file.javascript_member_is_stable(owner.symbol, member, target.symbol)
                {
                    return None;
                }
                owner = target;
                continue;
            }
            if owner.symbol.kind == "class" {
                owner =
                    self.javascript_class_member(owner, member, MemberMode::Static, role, work)?;
                continue;
            }
            let namespace = matches!(owner.symbol.kind, "namespace" | "enum" | "object");
            let object_value = matches!(owner.symbol.kind, "variable" | "property" | "object")
                && owner
                    .symbol
                    .body_range
                    .and_then(|range| file.text(range))
                    .is_some_and(|value| value.starts_with('{'));
            if !namespace && !object_value {
                return None;
            }
            let target = declared?;
            if object_value && !owner.symbol.body_range?.encloses(target.symbol.item_range) {
                return None;
            }
            if role == ReferenceRole::Call
                && object_value
                && !file.javascript_member_is_stable(owner.symbol, member, target.symbol)
            {
                return None;
            }
            owner = target;
        }
        Some(owner)
    }

    pub(super) fn javascript_declared_type(
        &self,
        file: &File<'source>,
        annotation: rift_syntax::ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut current = self.javascript_target(file, annotation, ReferenceRole::Type, work)?;
        let mut visited = BTreeSet::new();
        // Alias traversal shares the work budget and has a separate depth bound.
        for _ in 0..DECLARED_TYPE_DEPTH_MAX {
            if !work.charge()
                || !visited.insert((current.file.path, current.symbol.qualified_name.as_str()))
            {
                return None;
            }
            if matches!(current.symbol.kind, "class" | "interface") {
                return Some(current);
            }
            let source = self.files.get(current.file.path.as_str())?;
            let annotation = source
                .declared_types
                .get(&current.symbol.name_range?)?
                .as_ref()?;
            current = self.javascript_target(source, *annotation, ReferenceRole::Type, work)?;
        }
        None
    }

    fn javascript_class_receiver(
        &self,
        file: &File<'source>,
        position: u64,
        work: &mut Work,
    ) -> Option<(Target<'source>, MemberMode)> {
        let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
        let symbol = *scopes.classes.resolve(position, position.checked_add(1)?)?;
        let class = Target {
            file: file.document,
            symbol,
        };
        if scopes
            .functions
            .resolve(position, position.checked_add(1)?)
            .is_some_and(|range| class.symbol.range.encloses(*range))
        {
            return None;
        }
        let mut mode = file.javascript_member_mode(class, position, &scopes.definitions, work)?;

        if scopes
            .blocks
            .resolve(position, position.checked_add(1)?)
            .is_some_and(|range| class.symbol.range.encloses(*range))
        {
            mode = MemberMode::Static;
        }
        Some((class, mode))
    }

    pub(super) fn javascript_super_constructor(
        &self,
        file: &File<'source>,
        position: u64,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (class, mode) = self.javascript_class_receiver(file, position, work)?;
        if mode != MemberMode::Instance {
            return None;
        }
        let bases = self.javascript_bases(class, work)?;
        (bases.len() == 1).then(|| bases[0])
    }

    pub(super) fn javascript_receiver_member(
        &self,
        file: &File<'source>,
        position: u64,
        receiver: &str,
        member: &str,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (class, mode) = self.javascript_class_receiver(file, position, work)?;
        if receiver == "this" {
            return self.javascript_class_member(class, member, mode, role, work);
        }
        let mut found = None;
        for base in self.javascript_bases(class, work)? {
            let target = self.javascript_class_member(base, member, mode, role, work);
            if let Some(target) = target {
                if found.is_some() {
                    return None;
                }
                found = Some(target);
            }
        }
        found
    }

    pub(super) fn javascript_class_member(
        &self,
        class: Target<'source>,
        member: &str,
        mode: MemberMode,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (member, tail) = member
            .split_once('.')
            .map_or((member.trim(), None), |(head, tail)| {
                (head.trim(), Some(tail.trim()))
            });
        let mut pending = vec![class];
        let mut visited = BTreeSet::new();
        let mut found: Option<Target<'source>> = None;
        while let Some(class) = pending.pop() {
            if !work.charge() {
                return None;
            }
            if !visited.insert((class.file.path, class.symbol.qualified_name.as_str())) {
                continue;
            }
            let file = self.files.get(class.file.path.as_str())?;
            if role == ReferenceRole::Call
                && class.symbol.kind != "interface"
                && file.assignments.may_replace_method(member)
            {
                return None;
            }
            let target = file.javascript_declared_member(
                &format!("{}.{}", class.symbol.qualified_name, member),
                class.symbol.range,
                mode,
                role,
                work,
            );
            if let Some(target) = target {
                if found.is_some_and(|previous| {
                    previous.file.path != target.file.path
                        || previous.symbol.qualified_name != target.symbol.qualified_name
                }) {
                    return None;
                }
                found = Some(target);
            } else {
                pending.extend(self.javascript_bases(class, work)?);
            }
        }
        let found = found?;
        match tail {
            Some(tail) => self.javascript_object_member(found, tail, role, work),
            None => Some(found),
        }
    }

    pub(super) fn javascript_bases(
        &self,
        class: Target<'source>,
        work: &mut Work,
    ) -> Option<Vec<Target<'source>>> {
        let file = self.files.get(class.file.path.as_str())?;
        let mut bases = Vec::new();
        for edge in file.facts.heritage_edges() {
            if !work.charge() {
                return None;
            }
            if edge.kind == EdgeKind::Extends
                && Some(edge.from) == class.symbol.name_range
                && let Some(base) = self.edge_javascript_target(file, edge.target, work)
            {
                bases.push(base);
            }
        }
        Some(bases)
    }
}

impl<'source> File<'source> {
    pub(super) fn index_javascript_object_write(
        &mut self,
        member: &rift_syntax::StaticMember,
        work: &mut Work,
    ) {
        let Some(receiver) = self.text(member.receiver) else {
            return;
        };
        let binding = match self.binding(receiver, member.receiver.start, |_| true, work) {
            super::Binding::Found(binding)
                if binding.kind == rift_syntax::StaticBindingKind::Local =>
            {
                binding
            }
            super::Binding::Blocked => {
                self.unknown_member_writes.insert(Some(receiver));
                return;
            }
            _ => return,
        };
        let name = member.member.and_then(|range| self.text(range));
        self.member_writes
            .entry((binding.name, name))
            .and_modify(|position| {
                if *position != Some(member.range.start) {
                    *position = None;
                }
            })
            .or_insert(Some(member.range.start));
    }

    fn javascript_member_is_stable(
        &self,
        owner: &rift_syntax::SyntaxSymbol,
        member: &str,
        target: &rift_syntax::SyntaxSymbol,
    ) -> bool {
        if self.javascript_reassigned(&owner.name, None)
            || self.unknown_member_writes.contains(&None)
            || self
                .unknown_member_writes
                .contains(&Some(owner.name.as_str()))
        {
            return false;
        }
        let Some(name) = owner.name_range else {
            return false;
        };
        if self.member_writes.contains_key(&(name, None)) {
            return false;
        }
        match self.member_writes.get(&(name, Some(member))) {
            None => true,
            Some(Some(position)) => {
                target.syntax_element_kind == Some("assignment_expression")
                    && *position == target.item_range.start
            }
            Some(None) => false,
        }
    }

    pub(super) fn javascript_container(
        &self,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut found = None;
        for &symbol in self.by_qualified_name.get(name)? {
            if !work.charge() {
                return None;
            }
            if matches!(symbol.kind, "class" | "interface") {
                if found.is_some() {
                    return None;
                }
                found = Some(Target {
                    file: self.document,
                    symbol,
                });
            }
        }
        found
    }

    fn javascript_declared_member(
        &self,
        name: &str,
        class_range: rift_syntax::ByteRange,
        mode: MemberMode,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut found: Option<Target<'source>> = None;
        for &symbol in self.by_qualified_name.get(name)? {
            if !work.charge() {
                return None;
            }
            if !class_range.encloses(symbol.range)
                || symbol.kinds.contains(&SymbolKind::Static) != (mode == MemberMode::Static)
            {
                continue;
            }
            if let Some(previous) = found {
                if symbol.kind != previous.symbol.kind {
                    return None;
                }
                // Overload signatures share a member, but two bodies are ambiguous.
                if role != ReferenceRole::Type {
                    if symbol.kind != "method"
                        || (symbol.body_range.is_some() && previous.symbol.body_range.is_some())
                    {
                        return None;
                    }
                    if symbol.body_range.is_some() {
                        found = Some(Target {
                            file: self.document,
                            symbol,
                        });
                    }
                }
            } else {
                found = Some(Target {
                    file: self.document,
                    symbol,
                });
            }
        }
        found
    }

    fn javascript_member_mode(
        &self,
        class: Target<'source>,
        position: u64,
        definitions: &crate::EnclosingDefinitions<&'source rift_syntax::SyntaxSymbol>,
        work: &mut Work,
    ) -> Option<MemberMode> {
        let mut symbol = *definitions.resolve(position, position.checked_add(1)?)?;
        while let Some(container) = symbol.container.as_deref() {
            if !work.charge()
                || symbol.kind == "function"
                || (symbol.kind == "method" && container != class.symbol.qualified_name)
            {
                return None;
            }
            if container == class.symbol.qualified_name {
                return Some(if symbol.kinds.contains(&SymbolKind::Static) {
                    MemberMode::Static
                } else {
                    MemberMode::Instance
                });
            }
            symbol = self.qualified_target(container)?.symbol;
        }
        Some(MemberMode::Instance)
    }
}
