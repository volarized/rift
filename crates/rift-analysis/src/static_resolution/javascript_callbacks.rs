//! Types an unannotated callback parameter from the function type written for it.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_syntax::{ByteRange, StaticBinding, StaticCall, StaticFacts, StaticFunctionContext};

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
}

impl<'source> Callbacks<'source> {
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
        let context = *file.callbacks.contexts.get(&function)?;
        work.javascript_lookup(|work| {
            let (owner, written) = match context {
                StaticFunctionContext::Initializer(name) => {
                    self.javascript_function_type(file, name, work)?
                }
                StaticFunctionContext::Argument { call, index } => {
                    let call = file.callbacks.calls.get(&call).copied()?;
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
                StaticFunctionContext::Returned(enclosing) => {
                    let annotation = *file.callbacks.return_types.get(&enclosing)?;
                    self.javascript_named_function_type(file, annotation, work)?
                }
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

    /// The function type written for the declaration `name`, inline or through its named
    /// type.
    fn javascript_function_type<'index>(
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
}
