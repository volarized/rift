//! Resolves explicit ECMAScript receivers and statically named base members.

use std::collections::BTreeSet;

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};

use super::javascript::MemberMode;
use super::{File, Index, Target, Work};

/// Maximum parentheses, assertions and non-null marks unwrapped around a write receiver.
const RECEIVER_WRAPPERS_MAX: u32 = 8;

/// Maximum aliases followed while resolving an explicit receiver type.
pub(super) const DECLARED_TYPE_DEPTH_MAX: u32 = 32;

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
                if role == ReferenceRole::Type || !self.javascript_stable_class_value(owner) {
                    return None;
                }
                owner =
                    self.javascript_class_member(owner, member, MemberMode::Static, role, work)?;
                continue;
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
        let mut current = self.javascript_named_type(file, annotation, work)?;
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
            let Some(annotation) = source.declared_types.get(&current.symbol.name_range?) else {
                // An alias of a structural type declares its members itself.
                return (current.symbol.kind == "type_alias").then_some(current);
            };
            let annotation = annotation.as_ref()?;
            current = self.javascript_named_type(source, *annotation, work)?;
        }
        None
    }

    /// Whether a written type name is a type parameter in scope at its position.
    pub(super) fn javascript_type_parameter(
        &self,
        file: &File<'source>,
        annotation: rift_syntax::ByteRange,
        work: &mut Work,
    ) -> bool {
        file.text(annotation)
            .map(str::trim)
            .filter(|name| !name.contains('.'))
            .and_then(|name| {
                super::javascript::js_binding(
                    file,
                    &self.javascript,
                    name,
                    annotation.start,
                    ReferenceRole::Type,
                    work,
                )
            })
            .is_some_and(|binding| binding.kind == rift_syntax::StaticBindingKind::Shadow)
    }

    /// The declaration a written type name refers to, read through type parameters.
    ///
    /// A type parameter stands for its declared constraint: a value of that type has at
    /// least the constraint's members. A parameter without a constraint names nothing.
    fn javascript_named_type(
        &self,
        file: &File<'source>,
        mut annotation: rift_syntax::ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        for _ in 0..DECLARED_TYPE_DEPTH_MAX {
            let name = file.text(annotation)?.trim();
            let constraint = (!name.contains('.'))
                .then(|| {
                    super::javascript::js_binding(
                        file,
                        &self.javascript,
                        name,
                        annotation.start,
                        ReferenceRole::Type,
                        work,
                    )
                })
                .flatten()
                .filter(|binding| binding.kind == rift_syntax::StaticBindingKind::Shadow)
                .and_then(|binding| file.declared_types.get(&binding.name).copied().flatten());
            match constraint {
                Some(written) => annotation = written,
                None => {
                    return self.javascript_target(file, annotation, ReferenceRole::Type, work);
                }
            }
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
        let Some((class, mode)) = self.javascript_class_receiver(file, position, work) else {
            return (receiver == "this")
                .then(|| self.javascript_object_receiver(file, position, member, work))
                .flatten();
        };
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

    /// The member `member` of the object literal whose method holds `position`, for `this`
    /// outside every class. The method must be the nearest function that binds `this`; the
    /// member is a direct member of that literal, written once.
    fn javascript_object_receiver(
        &self,
        file: &File<'source>,
        position: u64,
        member: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let entries = file.facts.object_members();
        let mut holder: Option<(rift_syntax::ByteRange, rift_syntax::ByteRange)> = None;
        for &(function, object) in entries {
            if !work.charge() {
                return None;
            }
            if function.contains(position)
                && holder.is_none_or(|(inner, _)| inner.encloses(function))
            {
                holder = Some((function, object));
            }
        }
        let (function, object) = holder?;
        let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
        if scopes
            .functions
            .resolve(position, position.checked_add(1)?)
            .is_some_and(|inner| function.encloses(*inner) && *inner != function)
        {
            return None;
        }
        Self::javascript_literal_member(file, object, member, work)
    }

    /// The direct member `member` of the object literal at `object`, written once.
    pub(super) fn javascript_literal_member(
        file: &File<'source>,
        object: rift_syntax::ByteRange,
        member: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let entries = file.facts.object_members();
        let mut found: Option<&rift_syntax::SyntaxSymbol> = None;
        // Declarations whose name lies inside the literal, in source order.
        let names = rift_syntax::ByteRange {
            start: object.start,
            end: object.start,
        }..rift_syntax::ByteRange {
            start: object.end,
            end: object.end,
        };
        for symbol in file.by_range.range(names).filter_map(|(_, symbol)| *symbol) {
            if !work.charge() {
                return None;
            }
            let item = symbol.item_range;
            if symbol.name != member || !object.encloses(item) {
                continue;
            }
            // A local of a member function, or a member of a nested literal, is no member.
            let nested = entries.iter().any(|&(inner, other)| {
                (object.encloses(inner) && inner.encloses(item) && inner != item)
                    || (other != object && object.encloses(other) && other.encloses(item))
            });
            if nested {
                continue;
            }
            // A member and the named function written as its value share the name.
            found = match found {
                Some(previous) if previous.item_range.encloses(item) => Some(previous),
                Some(previous) if item.encloses(previous.item_range) => Some(symbol),
                Some(_) => return None,
                None => Some(symbol),
            };
        }
        Some(Target {
            file: file.document,
            symbol: found?,
        })
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
        let mut assigned: Option<Target<'source>> = None;
        let mut replaced = false;
        while let Some(class) = pending.pop() {
            if !work.charge() {
                return None;
            }
            if !visited.insert((class.file.path, class.symbol.qualified_name.as_str())) {
                continue;
            }
            let file = self.files.get(class.file.path.as_str())?;
            let target = file.javascript_declared_member(
                &format!("{}.{}", class.symbol.qualified_name, member),
                class.symbol.range,
                mode,
                role,
                work,
            );
            if role == ReferenceRole::Call && class.symbol.kind != "interface" {
                let field = target.filter(|target| {
                    tail.is_none()
                        && target.symbol.syntax_element_kind == Some("public_field_definition")
                });
                if self.javascript_member_replaced(file, class, member, field, work) {
                    tail?;
                    replaced = true;
                }
            }
            match target {
                // A member assigned through `this` names the declaration an ancestor
                // makes when one resolves (D46); otherwise it is the class's own facet.
                Some(target)
                    if target.symbol.syntax_element_kind == Some("assignment_expression") =>
                {
                    assigned.get_or_insert(target);
                    if let Some(bases) = self.javascript_bases(class, work) {
                        pending.extend(bases);
                    }
                }
                Some(target) => {
                    if found.is_some_and(|previous| {
                        previous.file.path != target.file.path
                            || previous.symbol.qualified_name != target.symbol.qualified_name
                    }) {
                        return None;
                    }
                    found = Some(target);
                }
                None => pending.extend(self.javascript_bases(class, work)?),
            }
        }
        let found = found.or(assigned)?;
        let Some(tail) = tail else {
            return Some(found);
        };
        // A member written with a type holds a value of that type whatever writes it, so
        // a call through it keeps the members of that type. A written member without a
        // type gives the call no target.
        if replaced
            && !found.symbol.name_range.is_some_and(|name| {
                self.files
                    .get(found.file.path.as_str())
                    .and_then(|file| file.declared_types.get(&name).copied().flatten())
                    .is_some()
            })
        {
            return None;
        }
        self.javascript_object_member(found, tail, role, work)
    }

    /// Whether a write in the class's file gives `member` of `class` another value.
    ///
    /// Only a write that names the member counts: a dotted name or a quoted literal key. A
    /// computed key states no member, so it leaves every declared member as declared.
    /// A write changes a member of the object its receiver denotes. Source establishes
    /// that object as this class through `this` inside the class, the class name, its
    /// `prototype`, or a binding declared or constructed as the class. Any other receiver
    /// names another object, whatever property it writes. A member stored back bound to its
    /// own receiver is the same member.
    fn javascript_member_replaced(
        &self,
        file: &File<'source>,
        class: Target<'source>,
        member: &str,
        field: Option<Target<'source>>,
        work: &mut Work,
    ) -> bool {
        for &(start, target) in &file.assignments.member_writes {
            if !work.charge() {
                return true;
            }
            let Some((receiver, property)) = super::written_member(target) else {
                continue;
            };
            // A computed key names no member: source does not state which one it writes.
            let Some(property) = property else {
                continue;
            };
            if ![member, "constructor", "prototype"].contains(&property) {
                continue;
            }
            let end = start.saturating_add(target.len() as u64);
            if property == member
                && super::rebinds_itself(file.document.source, target, receiver, end)
            {
                continue;
            }
            // A function or an object literal written in place through `this` declares
            // another facet of the member (D38): the call still targets the member. Writes
            // through other receivers keep the refusal.
            if property == member
                && receiver.trim() == "this"
                && file.function_writes.contains(&start)
            {
                continue;
            }
            if !self.javascript_receiver_denotes(file, class, receiver, start, work) {
                continue;
            }
            // The written signature establishes a direct call after a write through
            // `this` or a checked copy of the same field between instances.
            if property == member
                && !matches!(property, "constructor" | "prototype")
                && field.is_some_and(|field| {
                    (receiver.trim() == "this"
                        || (!field.symbol.kinds.contains(&SymbolKind::Static)
                            && self
                                .javascript_same_field_copy(
                                    file,
                                    class,
                                    rift_syntax::ByteRange { start, end },
                                    member,
                                    work,
                                )
                                .is_some()))
                        && self.javascript_declared_signature(field, false, work)
                })
            {
                continue;
            }
            return true;
        }
        false
    }

    /// Whether this write copies the same declared field between instances of one class.
    fn javascript_same_field_copy(
        &self,
        file: &File<'source>,
        class: Target<'source>,
        target: rift_syntax::ByteRange,
        member: &str,
        work: &mut Work,
    ) -> Option<()> {
        let copies = file.facts.member_copies();
        let position = copies
            .binary_search_by_key(&target.start, |copy| copy.target.start)
            .ok()?;
        let copy = copies.get(position).filter(|copy| copy.target == target)?;
        if file.blocked(target.start, work) {
            return None;
        }
        let written = file.members.get(&copy.target).copied().flatten()?;
        let value = file.members.get(&copy.value).copied().flatten()?;
        if file.text(value.receiver)? != "this"
            || file.text(written.member?)? != member
            || file.text(value.member?)? != member
        {
            return None;
        }
        let name = file.text(written.receiver)?;
        let binding = super::javascript::js_binding(
            file,
            &self.javascript,
            name,
            written.receiver.start,
            ReferenceRole::Read,
            work,
        )?;
        let rift_syntax::StaticBindingKind::JavaScriptConstInstance { constructor } = binding.kind
        else {
            return None;
        };
        if file
            .assignments
            .name_writes
            .get(name)
            .is_some_and(|writes| {
                writes
                    .iter()
                    .any(|&write| !work.charge() || binding.scope.contains(write))
            })
        {
            return None;
        }
        let (source, MemberMode::Instance) =
            self.javascript_class_receiver(file, value.receiver.start, work)?
        else {
            return None;
        };
        let destination = self.javascript_target(file, constructor, ReferenceRole::Call, work)?;
        [source, destination]
            .into_iter()
            .all(|owner| {
                owner.file.path == class.file.path && std::ptr::eq(owner.symbol, class.symbol)
            })
            .then_some(())
    }

    fn javascript_receiver_denotes(
        &self,
        file: &File<'source>,
        class: Target<'source>,
        receiver: &str,
        position: u64,
        work: &mut Work,
    ) -> bool {
        let same = |target: Target<'source>| {
            target.file.path == class.file.path && std::ptr::eq(target.symbol, class.symbol)
        };
        let mut receiver = receiver.trim();
        // `(this as any)` and `item!` denote the same object as the expression they wrap.
        for _ in 0..RECEIVER_WRAPPERS_MAX {
            let inner = receiver
                .strip_prefix('(')
                .and_then(|inner| inner.strip_suffix(')'))
                .map(|inner| inner.split(" as ").next().unwrap_or(inner))
                .or_else(|| receiver.strip_suffix('!'));
            match inner {
                Some(inner) => receiver = inner.trim(),
                None => break,
            }
        }
        if receiver == "this" {
            return self
                .javascript_class_receiver(file, position, work)
                .is_some_and(|(owner, _)| same(owner));
        }
        let name = receiver
            .strip_suffix(".prototype")
            .unwrap_or(receiver)
            .trim();
        if name.contains(['.', '[', '(', ' ']) {
            return false;
        }
        let Some(binding) = super::javascript::js_binding(
            file,
            &self.javascript,
            name,
            position,
            ReferenceRole::Read,
            work,
        ) else {
            return false;
        };
        let written = file.target_at(binding.name);
        if written.is_some_and(same) {
            return true;
        }
        if name.len() != receiver.len() {
            return false;
        }
        // A variable that still holds a class's original value writes the same
        // object. Resolve a bare initializer name in the scope where it was written.
        if let Some(value) = written
            .filter(|target| target.symbol.syntax_element_kind == Some("variable_declarator"))
            .and_then(|target| target.symbol.body_range)
            && !file.javascript_binding_reassigned(binding)
            && let Some(name) = file.text(value)
            && let Some(source) = super::javascript::js_binding(
                file,
                &self.javascript,
                name,
                value.start,
                ReferenceRole::Read,
                work,
            )
            && file.target_at(source.name).is_some_and(same)
        {
            return true;
        }
        if let Some(&Some(annotation)) = file.declared_types.get(&binding.name) {
            return self
                .javascript_declared_type(file, annotation, work)
                .is_some_and(same);
        }
        match binding.kind {
            rift_syntax::StaticBindingKind::JavaScriptInstance { constructor }
            | rift_syntax::StaticBindingKind::JavaScriptConstInstance { constructor } => self
                .javascript_target(file, constructor, ReferenceRole::Call, work)
                .is_some_and(same),
            _ => false,
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
                && let Some(base) = self.javascript_heritage_target(file, edge, work)
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
                if matches!(
                    binding.kind,
                    rift_syntax::StaticBindingKind::Local
                        | rift_syntax::StaticBindingKind::JavaScriptClassValue { .. }
                ) =>
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
        // A member assigned through `this` is another facet of a declared member (D38); it
        // stands for the member only when nothing declares it.
        let mut assigned: Option<Target<'source>> = None;
        for &symbol in self.by_qualified_name.get(name)? {
            if !work.charge() {
                return None;
            }
            if !class_range.encloses(symbol.range)
                || symbol.kinds.contains(&SymbolKind::Static) != (mode == MemberMode::Static)
            {
                continue;
            }
            if symbol.syntax_element_kind == Some("assignment_expression") {
                assigned.get_or_insert(Target {
                    file: self.document,
                    symbol,
                });
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
        found.or(assigned)
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
            let Some(parent) = self.qualified_target(container) else {
                // The container has no single declaration, as a member of an object
                // literal beside a declared return type of the same shape. The class
                // member that holds the position decides.
                return self.javascript_holding_member_mode(class, position, work);
            };
            symbol = parent.symbol;
        }
        Some(MemberMode::Instance)
    }

    /// The mode of the member of `class` whose source holds `position`.
    fn javascript_holding_member_mode(
        &self,
        class: Target<'source>,
        position: u64,
        work: &mut Work,
    ) -> Option<MemberMode> {
        let mut mode = MemberMode::Instance;
        for symbol in self.document.facts.symbols() {
            if !work.charge() {
                return None;
            }
            if symbol.container.as_deref() == Some(class.symbol.qualified_name.as_str())
                && class.symbol.range.encloses(symbol.range)
                && symbol.range.contains(position)
                && symbol.kinds.contains(&SymbolKind::Static)
            {
                mode = MemberMode::Static;
            }
        }
        Some(mode)
    }
}
