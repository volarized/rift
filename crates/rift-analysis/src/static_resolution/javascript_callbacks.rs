//! Types an unannotated callback parameter from the function type written for it.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_syntax::{
    ByteRange, StaticBinding, StaticCall, StaticFacts, StaticFunctionContext, StaticMemberCallback,
};

use super::javascript_members::DECLARED_TYPE_DEPTH_MAX;
use super::{File, Index, Target, Work};

/// One entry of a declared return type: the member of an inline object type, or none for
/// a named type, and the type written for it.
type DeclaredReturn = (Option<ByteRange>, ByteRange);

/// Written facts that connect a callback parameter to its function type.
#[derive(Default)]
pub(super) struct Callbacks<'source> {
    /// Parameter name to the function that declares it and its position.
    positions: BTreeMap<ByteRange, (ByteRange, u32)>,
    /// Function and position to the parameter name.
    names: BTreeMap<(ByteRange, u32), ByteRange>,
    /// Owning declaration name and position to the parameter name; absent when two
    /// functions share the owner, as overloads do.
    owned: BTreeMap<(ByteRange, u32), Option<ByteRange>>,
    contexts: BTreeMap<ByteRange, StaticFunctionContext>,
    member_initializers: BTreeMap<ByteRange, &'source StaticMemberCallback>,
    member_type_shapes: BTreeMap<ByteRange, ByteRange>,
    return_types: BTreeMap<ByteRange, ByteRange>,
    /// Declaration name to its written function type; absent when it writes several.
    function_types: BTreeMap<ByteRange, Option<ByteRange>>,
    calls: BTreeMap<ByteRange, &'source StaticCall>,
    /// Name bound from a call to its key and the call; no key when the name takes the
    /// whole result.
    destructured: BTreeMap<ByteRange, (Option<ByteRange>, ByteRange)>,
    /// Owning declaration name to its declared return entries; absent when two functions
    /// share the owner, as overloads do.
    returns: BTreeMap<ByteRange, Option<Vec<DeclaredReturn>>>,
    /// Owning declaration name to the element type of the array its function returns;
    /// absent when two functions share the owner.
    element_returns: BTreeMap<ByteRange, Option<ByteRange>>,
    /// Owning declaration name to what each `return` of its function returns: the object
    /// literal, or none for any other value.
    returned_objects: BTreeMap<ByteRange, Vec<Option<ByteRange>>>,
}

/// What a standard array method gives one of its callback's parameters.
enum ArrayCallback<'source> {
    /// The call is not an array method call on a receiver with a declared array type.
    Other,
    /// The element type at an element position; nothing at another position.
    Element(Option<Target<'source>>),
}

/// Standard array methods whose callback walks the array's elements, with the callback
/// parameter positions that receive an element.
const ARRAY_CALLBACKS: [(&str, &[u32]); 12] = [
    ("forEach", &[0]),
    ("map", &[0]),
    ("filter", &[0]),
    ("find", &[0]),
    ("findIndex", &[0]),
    ("findLast", &[0]),
    ("some", &[0]),
    ("every", &[0]),
    ("flatMap", &[0]),
    ("reduce", &[1]),
    ("reduceRight", &[1]),
    ("sort", &[0, 1]),
];

impl<'source> Callbacks<'source> {
    /// Whether `range` is a call.
    pub(super) fn is_call(&self, range: ByteRange) -> bool {
        self.calls.contains_key(&range)
    }

    /// A receiver participates directly in a written named member call.
    pub(super) fn has_named_member_call(
        &self,
        receiver: ByteRange,
        work: &mut Work,
        accepts: impl Fn(ByteRange) -> bool,
    ) -> bool {
        self.calls
            .range(receiver..)
            .take_while(|(range, _)| range.start == receiver.start)
            .any(|(_, call)| {
                work.charge()
                    && matches!(
                        call.callee,
                        rift_syntax::StaticCallee::Member { receiver: found, member: property }
                            if found == receiver && accepts(property)
                    )
            })
    }

    /// Whether the declaration `name` is written with one function type.
    pub(super) fn has_function_type(&self, name: ByteRange) -> bool {
        self.function_types.get(&name).is_some_and(Option::is_some)
    }

    pub(super) fn member_type_shape(&self, name: ByteRange) -> Option<ByteRange> {
        self.member_type_shapes.get(&name).copied()
    }

    pub(super) fn index(facts: &'source StaticFacts, work: &mut Work) -> Option<Self> {
        let mut callbacks = Self::default();
        let mut owners = BTreeMap::<ByteRange, Option<ByteRange>>::new();
        for parameter in facts.parameters() {
            if !work.charge() {
                return None;
            }
            callbacks
                .positions
                .insert(parameter.name, (parameter.function, parameter.index));
            callbacks
                .names
                .insert((parameter.function, parameter.index), parameter.name);
            if let Some(owner) = parameter.owner {
                // Two functions under one name are overloads: neither types a callback.
                let single = *owners.entry(owner).or_insert(Some(parameter.function))
                    == Some(parameter.function);
                if !single {
                    owners.insert(owner, None);
                }
                callbacks
                    .owned
                    .entry((owner, parameter.index))
                    .and_modify(|name| {
                        if *name != Some(parameter.name) {
                            *name = None;
                        }
                    })
                    .or_insert(Some(parameter.name));
            }
        }
        callbacks
            .owned
            .retain(|(owner, _), _| owners.get(owner).copied().flatten().is_some());
        for &(function, context) in facts.function_contexts() {
            if !work.charge() {
                return None;
            }
            callbacks.contexts.insert(function, context);
        }
        for context in facts.member_callbacks() {
            if !work.charge() {
                return None;
            }
            callbacks
                .member_initializers
                .insert(context.parameter, context);
        }
        for &(name, body) in facts.member_type_shapes() {
            if !work.charge() {
                return None;
            }
            callbacks.member_type_shapes.insert(name, body);
        }
        for &(function, annotation) in facts.return_types() {
            if !work.charge() {
                return None;
            }
            callbacks.return_types.insert(function, annotation);
        }
        for &(name, written) in facts.function_types() {
            if !work.charge() {
                return None;
            }
            callbacks
                .function_types
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(written));
        }
        for call in facts.calls() {
            if !work.charge() {
                return None;
            }
            callbacks.calls.insert(call.range, call);
        }
        for &(name, key, call) in facts.destructured() {
            if !work.charge() {
                return None;
            }
            callbacks.destructured.insert(name, (key, call));
        }
        callbacks.index_returns(facts, work)?;
        let mut writers = BTreeMap::<ByteRange, ByteRange>::new();
        for declared in facts.declared_returns() {
            if !work.charge() {
                return None;
            }
            // Two functions under one name are overloads: neither types the result.
            let single =
                *writers.entry(declared.owner).or_insert(declared.function) == declared.function;
            let entries = callbacks
                .returns
                .entry(declared.owner)
                .or_insert_with(|| Some(Vec::new()));
            if !single {
                *entries = None;
            } else if let Some(entries) = entries {
                entries.push((declared.member, declared.annotation));
            }
        }
        Some(callbacks)
    }

    /// Indexes what each function returns: the object literals of its `return`s and the
    /// element type of a written array return type.
    fn index_returns(&mut self, facts: &'source StaticFacts, work: &mut Work) -> Option<()> {
        for &(owner, object) in facts.returned_objects() {
            if !work.charge() {
                return None;
            }
            self.returned_objects.entry(owner).or_default().push(object);
        }
        for &(owner, element) in facts.element_returns() {
            if !work.charge() {
                return None;
            }
            self.element_returns
                .entry(owner)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(element));
        }
        Some(())
    }
}

impl<'source> Index<'source> {
    /// The class or interface an unannotated callback parameter is declared as.
    ///
    /// The function type comes from the place that types the function value: the declared
    /// type of the declaration it initializes, the declared parameter of the resolved
    /// callee at its argument position, or the declared return type of the function that
    /// returns it. The parameter takes the named type written at its own position in that
    /// function type. An unresolved or overloaded callee, a type that is no function type,
    /// a position without a named type and a position typed by a type parameter leave the
    /// parameter untyped.
    pub(super) fn javascript_callback_parameter<'index>(
        &'index self,
        file: &'index File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let &(function, index) = file.callbacks.positions.get(&binding.name)?;
        let context = file.callbacks.contexts.get(&function).copied();
        work.javascript_lookup(|work| {
            let (owner, written) = match context {
                Some(StaticFunctionContext::Initializer(name)) => {
                    self.javascript_function_type(file, name, work)?
                }
                Some(StaticFunctionContext::Argument {
                    call,
                    index: argument,
                }) => {
                    let call = file.callbacks.calls.get(&call).copied()?;
                    if let ArrayCallback::Element(element) =
                        self.javascript_array_callback(file, call, argument, index, work)
                    {
                        return element;
                    }
                    let index = argument;
                    let callee = self.javascript_call(file, call, work)?;
                    let declared = self.files.get(callee.file.path.as_str())?;
                    let parameter = declared
                        .callbacks
                        .owned
                        .get(&(callee.symbol.name_range?, index))
                        .copied()
                        .flatten()?;
                    self.javascript_function_type(declared, parameter, work)?
                }
                Some(StaticFunctionContext::Returned(enclosing)) => {
                    let annotation = *file.callbacks.return_types.get(&enclosing)?;
                    self.javascript_named_function_type(file, annotation, work)?
                }
                None => self.javascript_member_callback(
                    file,
                    file.callbacks.member_initializers.get(&binding.name)?,
                    work,
                )?,
            };
            let parameter = owner.callbacks.names.get(&(written, index))?;
            let annotation = owner.declared_types.get(parameter).copied().flatten()?;
            if self.javascript_type_parameter(owner, annotation, work) {
                // The call decides a type parameter; its constraint is not that type.
                return None;
            }
            self.javascript_declared_type(owner, annotation, work)
        })
    }

    /// The class or interface a name bound from a call is declared as.
    ///
    /// A name that takes the whole result takes the named return type the resolved callee
    /// declares. A destructured name reads its key in that declared return type: the
    /// member of an inline object type, or the member a named return type declares. A
    /// callee without a declared return type, an overloaded callee, a key the type does
    /// not declare and a name assigned again leave the name untyped.
    pub(super) fn javascript_destructured_name<'index>(
        &'index self,
        file: &'index File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let &(key, call) = file.callbacks.destructured.get(&binding.name)?;
        if file.javascript_binding_reassigned(binding) {
            return None;
        }
        let key = match key {
            Some(key) => Some(file.text(key)?),
            None => None,
        };
        let call = file.callbacks.calls.get(&call).copied()?;
        work.javascript_lookup(|work| {
            let callee = self.javascript_call(file, call, work)?;
            let declared = self.files.get(callee.file.path.as_str())?;
            let entries = declared
                .callbacks
                .returns
                .get(&callee.symbol.name_range?)?
                .as_ref()?;
            for &(member, annotation) in entries {
                if !work.charge() {
                    return None;
                }
                match (member, key) {
                    (Some(member), Some(key)) if declared.text(member) == Some(key) => {
                        return self.javascript_declared_type(declared, annotation, work);
                    }
                    (Some(_), _) => {}
                    (None, None) => {
                        return self.javascript_declared_type(declared, annotation, work);
                    }
                    (None, Some(key)) => {
                        let owner = self.javascript_declared_type(declared, annotation, work)?;
                        let member = self.javascript_class_member(
                            owner,
                            key,
                            super::javascript::MemberMode::Instance,
                            ReferenceRole::Read,
                            work,
                        )?;
                        let holder = self.files.get(member.file.path.as_str())?;
                        let written = holder
                            .declared_types
                            .get(&member.symbol.name_range?)
                            .copied()
                            .flatten()?;
                        return self.javascript_declared_type(holder, written, work);
                    }
                }
            }
            None
        })
    }

    /// Whether the binding holds a call result the resolved callee declares callable.
    ///
    /// `const handler = some()` with `some(): Handler` and `Handler` a function type: the
    /// call targets the binding `handler`; no function body is chosen for it. A callee
    /// without a declared return type, a return type written as a type parameter and a
    /// binding assigned again leave the binding uncalled.
    pub(super) fn javascript_callable_result(
        &self,
        target: Target<'source>,
        work: &mut Work,
    ) -> bool {
        let Some(file) = self.files.get(target.file.path.as_str()) else {
            return false;
        };
        let Some(&(None, call)) = target
            .symbol
            .name_range
            .and_then(|name| file.callbacks.destructured.get(&name))
        else {
            return false;
        };
        let written = target
            .symbol
            .name_range
            .and_then(|name| file.text(name))
            .is_none_or(|name| file.javascript_reassigned(name, None));
        let Some(call) = file
            .callbacks
            .calls
            .get(&call)
            .copied()
            .filter(|_| !written)
        else {
            return false;
        };
        work.javascript_lookup(|work| {
            let callee = self.javascript_call(file, call, work)?;
            let declared = self.files.get(callee.file.path.as_str())?;
            let entries = declared
                .callbacks
                .returns
                .get(&callee.symbol.name_range?)?
                .as_ref()?;
            let [(None, annotation)] = entries.as_slice() else {
                return None;
            };
            self.javascript_written_signature(declared, *annotation, false, work)
                .then_some(())
        })
        .is_some()
    }

    /// The function type written for the declaration `name`, inline or through its named
    /// type.
    pub(super) fn javascript_function_type<'index>(
        &'index self,
        file: &'index File<'source>,
        name: ByteRange,
        work: &mut Work,
    ) -> Option<(&'index File<'source>, ByteRange)> {
        if let Some(written) = file.callbacks.function_types.get(&name) {
            return Some((file, (*written)?));
        }
        let annotation = file.declared_types.get(&name).copied().flatten()?;
        self.javascript_named_function_type(file, annotation, work)
    }

    /// The function type a named type writes, through at most
    /// `DECLARED_TYPE_DEPTH_MAX` aliases.
    fn javascript_named_function_type<'index>(
        &'index self,
        mut file: &'index File<'source>,
        mut annotation: ByteRange,
        work: &mut Work,
    ) -> Option<(&'index File<'source>, ByteRange)> {
        for _ in 0..DECLARED_TYPE_DEPTH_MAX {
            if !work.charge() {
                return None;
            }
            let named = self.javascript_target(file, annotation, ReferenceRole::Type, work)?;
            let owner = self.files.get(named.file.path.as_str())?;
            let name = named.symbol.name_range?;
            if let Some(written) = owner.callbacks.function_types.get(&name) {
                return Some((owner, (*written)?));
            }
            if named.symbol.kind != "type_alias" {
                return None;
            }
            annotation = owner.declared_types.get(&name).copied().flatten()?;
            file = owner;
        }
        None
    }

    /// What a standard array method gives a callback parameter: the call is
    /// `receiver.method(callback)` with `method` an element-walking array method, the
    /// callback its first argument and the receiver's declared type an array. An element
    /// position takes the element type; another position takes nothing.
    fn javascript_array_callback(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        argument: u32,
        parameter: u32,
        work: &mut Work,
    ) -> ArrayCallback<'source> {
        let rift_syntax::StaticCallee::Member { receiver, member } = call.callee else {
            return ArrayCallback::Other;
        };
        let Some(positions) = ARRAY_CALLBACKS
            .iter()
            .find(|(name, _)| file.text(member) == Some(*name))
            .map(|(_, positions)| *positions)
            .filter(|_| argument == 0)
        else {
            return ArrayCallback::Other;
        };
        let Some((owner, element)) = self.javascript_element_type(file, receiver, work) else {
            // An array method on a receiver of another type is that type's own method.
            return ArrayCallback::Other;
        };
        ArrayCallback::Element(
            positions
                .contains(&parameter)
                .then(|| self.javascript_declared_type(owner, element, work))
                .flatten(),
        )
    }

    /// The file and written element type of the array a receiver holds: a name declared
    /// with an array type, a field declared with one, or a call whose callee declares an
    /// array return type.
    pub(super) fn javascript_element_type<'index>(
        &'index self,
        file: &'index File<'source>,
        receiver: ByteRange,
        work: &mut Work,
    ) -> Option<(&'index File<'source>, ByteRange)> {
        if file.callbacks.is_call(receiver) {
            let call = file.callbacks.calls.get(&receiver).copied()?;
            return work.javascript_lookup(|work| {
                let callee = self.javascript_call(file, call, work)?;
                let declared = self.files.get(callee.file.path.as_str())?;
                let element = declared
                    .callbacks
                    .element_returns
                    .get(&callee.symbol.name_range?)
                    .copied()
                    .flatten()?;
                Some((declared, element))
            });
        }
        if let Some(member) = file.members.get(&receiver).copied().flatten() {
            let name = file.text(member.member?)?;
            let owner = self.javascript_receiver_class(file, member.receiver, work)?;
            return self.javascript_field_element(owner, name, work);
        }
        let name = file.text(receiver)?.trim();
        let binding = super::javascript::js_binding(
            file,
            &self.javascript,
            name,
            receiver.start,
            ReferenceRole::Read,
            work,
        )?;
        let element = file.element_types.get(&binding.name).copied().flatten()?;
        Some((file, element))
    }

    /// The class a member receiver denotes: `this` inside a class, or a name declared as
    /// a class or interface.
    fn javascript_receiver_class(
        &self,
        file: &File<'source>,
        receiver: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let text = file.text(receiver)?.trim();
        if text == "this" {
            let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
            let symbol = *scopes
                .classes
                .resolve(receiver.start, receiver.start.checked_add(1)?)?;
            return Some(Target {
                file: file.document,
                symbol,
            });
        }
        let binding = super::javascript::js_binding(
            file,
            &self.javascript,
            text,
            receiver.start,
            ReferenceRole::Read,
            work,
        )?;
        let annotation = file.declared_types.get(&binding.name).copied().flatten()?;
        self.javascript_declared_type(file, annotation, work)
    }

    /// The written element type of the field `name` of `owner`: its declaration's array
    /// type, or the documented array type of a `this.name` assignment inside the class.
    fn javascript_field_element<'index>(
        &'index self,
        owner: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<(&'index File<'source>, ByteRange)> {
        let file = self.files.get(owner.file.path.as_str())?;
        if let Some(element) = file
            .qualified_target(&format!("{}.{name}", owner.symbol.qualified_name))
            .and_then(|field| field.symbol.name_range)
            .and_then(|range| file.element_types.get(&range).copied().flatten())
        {
            return Some((file, element));
        }
        let mut found = None;
        for &(written, element) in file.facts.element_types() {
            if !work.charge() {
                return None;
            }
            // The written name is the property of `this.name`.
            let receiver = ByteRange {
                start: written.start.saturating_sub(5),
                end: written.start,
            };
            let assigned = owner.symbol.range.encloses(written)
                && file.text(written) == Some(name)
                && file.text(receiver) == Some("this.");
            if assigned
                && found
                    .replace(element)
                    .is_some_and(|previous| previous != element)
            {
                return None;
            }
        }
        Some((file, found?))
    }

    /// The member `member` of the value a name bound to a call holds, read from the object
    /// literals the callee returns (D38): the callee declares no return type, every one of
    /// its `return`s returns an object literal, and each of them writes the member. The
    /// member of the first literal is the read target. A call through explicit pairs
    /// uses their local callable when every literal proves the same declaration.
    pub(super) fn javascript_returned_member<'index>(
        &'index self,
        file: &'index File<'source>,
        binding: &StaticBinding,
        member: &str,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let &(None, call) = file.callbacks.destructured.get(&binding.name)? else {
            return None;
        };
        if file.javascript_binding_reassigned(binding) {
            return None;
        }
        let pair_unchanged = file
            .text(binding.name)
            .is_some_and(|receiver| !file.javascript_reassigned(receiver, Some(member)));
        self.javascript_member_of_returned_call(file, call, member, role, pair_unchanged, work)
    }

    /// Uses the checked authored object literals returned by the selected call.
    pub(super) fn javascript_member_of_returned_call(
        &self,
        file: &File<'source>,
        call: ByteRange,
        member: &str,
        role: ReferenceRole,
        pair_unchanged: bool,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let call = file.callbacks.calls.get(&call).copied()?;
        work.javascript_lookup(|work| {
            let callee = self.javascript_call(file, call, work)?;
            let declared = self.files.get(callee.file.path.as_str())?;
            let owner = callee.symbol.name_range?;
            if declared.callbacks.returns.contains_key(&owner)
                || declared.callbacks.element_returns.contains_key(&owner)
            {
                return None;
            }
            let objects = declared.callbacks.returned_objects.get(&owner)?;
            let mut first = None;
            let mut pair_target: Option<Target<'source>> = None;
            let mut resolve_pair = pair_unchanged
                && role == ReferenceRole::Call
                && file.document.facts.language().name == "javascript"
                && declared.document.facts.language().name == "javascript";
            for object in objects {
                let object = (*object)?;
                let target = Self::javascript_literal_member(declared, object, member, work)?;
                if first.is_none() && resolve_pair {
                    resolve_pair = target.symbol.syntax_element_kind == Some("pair");
                }
                first.get_or_insert(target);
                if resolve_pair {
                    let next = self.javascript_final_pair_callable(declared, target, object, work);
                    resolve_pair = next.is_some_and(|callable| {
                        // Both lexical targets are local to the callee's captured source.
                        pair_target.is_none_or(|previous| {
                            previous.symbol.item_range == callable.symbol.item_range
                        })
                    });
                    pair_target = next;
                }
            }
            pair_target.filter(|_| resolve_pair).or(first)
        })
    }

    /// The class or interface the result of the call at `range` is declared as: the named
    /// return type the resolved callee declares, or the class a `new` constructs. A callee
    /// without a declared return type, an overloaded callee and a return type written as a
    /// type parameter type nothing.
    pub(super) fn javascript_call_result<'index>(
        &'index self,
        file: &'index File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let call = file.callbacks.calls.get(&range).copied()?;
        work.javascript_lookup(|work| {
            let callee = self.javascript_call(file, call, work)?;
            if call.callee == rift_syntax::StaticCallee::Constructor {
                // `new C()` is an instance of the class it constructs.
                return (callee.symbol.kind == "class").then_some(callee);
            }
            let declared = self.files.get(callee.file.path.as_str())?;
            let entries = declared
                .callbacks
                .returns
                .get(&callee.symbol.name_range?)?
                .as_ref()?;
            let [(None, annotation)] = entries.as_slice() else {
                return None;
            };
            if self.javascript_type_parameter(declared, *annotation, work) {
                return None;
            }
            self.javascript_declared_type(declared, *annotation, work)
        })
    }
}
