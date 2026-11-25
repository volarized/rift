//! Method lookup on receivers whose type is a reference, a slice or an array.
//!
//! The compiler probes the type of the receiver, then each type it dereferences to, and an
//! array last as a slice. At each type it takes a method whose `self` has that type, then one
//! whose `self` is `&` and then `&mut` of it. The first probe with a method decides, and there
//! an inherent method comes before a trait method.

mod standard;

use rift_syntax::{ByteRange, StaticBindingKind};

use super::references::split_top_level;
use super::{
    Method, Namespace, Namespaces, RECEIVER_DEPTH_MAX, SelfKind, is_type_binding, is_value_binding,
    path_segments, receiver_form, self_kind, structural_owner, trait_method,
};
use crate::static_resolution::external::{RUST_RUNTIME_CRATES, rust_prelude};
use crate::static_resolution::{Binding, File, Target, Work};

/// Types one written type dereferences to through standard pointers and containers.
const BASES_MAX: usize = 16;

/// Standard types that dereference to their first type argument.
const POINTERS: [&str; 12] = [
    "borrow::Cow",
    "boxed::Box",
    "cell::LazyCell",
    "cell::Ref",
    "cell::RefMut",
    "mem::ManuallyDrop",
    "rc::Rc",
    "sync::Arc",
    "sync::LazyLock",
    "sync::MutexGuard",
    "sync::RwLockReadGuard",
    "sync::RwLockWriteGuard",
];

/// One reference in front of a type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reference {
    Shared,
    Unique,
}

/// The references in front of a base of the receiver, the outermost first.
#[derive(Clone, PartialEq, Eq)]
struct Probed {
    references: Vec<Reference>,
    base: usize,
}

/// A type without references in front of it.
struct Base {
    /// References between the type before it and this one.
    references: Vec<Reference>,
    /// Names under which the standard library declares its methods.
    names: Vec<String>,
}

/// A method the probes can take, with the type of its `self`.
struct Candidate<'source> {
    receiver: Probed,
    inherent: bool,
    /// Absent for a method of the standard library.
    method: Option<Method<'source>>,
}

impl<'source> Namespaces<'source> {
    /// The package method `member` names on a receiver of `owner`.
    ///
    /// `receiver` is the written receiver when it is a value of `owner`, or a reference to
    /// one. A method the standard library declares has no package target, so a site where it
    /// decides the probe resolves to nothing, as does one it could decide for a receiver
    /// whose references are not written.
    pub(super) fn receiver_method_of(
        &self,
        file: &File<'source>,
        receiver: Option<ByteRange>,
        position: u64,
        (owner, member): (Target<'source>, &str),
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if !structural_owner(owner) {
            return self.inherent_method(file, position, owner, member, work);
        }
        let bases = self.written_bases(file, owner, work)?;
        let mut candidates = Vec::new();
        for (base, names) in bases.iter().enumerate() {
            for name in &names.names {
                for references in standard::receivers(name, member) {
                    candidates.push(Candidate {
                        receiver: Probed {
                            references: references.to_vec(),
                            base,
                        },
                        inherent: true,
                        method: None,
                    });
                }
            }
        }
        if self.referent(owner).is_none() && candidates.is_empty() {
            return self.inherent_method(file, position, owner, member, work);
        }
        let before = receiver.and_then(|receiver| receiver_references(file, receiver, work));
        let mut references = match before {
            Some(references) => references,
            // The value has the type of the owner itself.
            None if candidates.is_empty() => Vec::new(),
            None => return None,
        };
        let written = &bases.first()?.references;
        references.extend(written.iter().copied());
        self.package_candidates((owner, member), written, &mut candidates, work)?;
        let mut probed = Vec::new();
        for (base, declared) in bases.iter().enumerate() {
            let mut references = if base == 0 {
                references.clone()
            } else {
                declared.references.clone()
            };
            loop {
                let last = references.is_empty();
                probed.push(Probed {
                    references: references.clone(),
                    base,
                });
                if last {
                    break;
                }
                references.remove(0);
            }
        }
        let target = probe(file, position, &probed, &candidates)?.target;
        (target.symbol.visibility.as_deref() == Some("pub")
            || self.declaration_visible(file, position, target, work)
            || target.symbol.visibility.is_none())
        .then_some(target)
    }

    /// Whether `Self::member` in an implementation for `owner` is an inherent method of the
    /// standard library. A path probes the written type alone.
    pub(super) fn standard_associated(
        &self,
        file: &File<'source>,
        owner: Target<'source>,
        member: &str,
        work: &mut Work,
    ) -> Option<bool> {
        let bases = self.written_bases(file, owner, work)?;
        Some(bases.first().is_some_and(|base| {
            base.references.is_empty()
                && base
                    .names
                    .iter()
                    .any(|name| standard::receivers(name, member).next().is_some())
        }))
    }

    /// The package methods `member` of a structural owner, and of its nominal referent.
    fn package_candidates(
        &self,
        (owner, member): (Target<'source>, &str),
        written: &[Reference],
        candidates: &mut Vec<Candidate<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        let unique = owner.symbol.name.starts_with("&mut");
        let mut owners = Vec::new();
        if let Some(referent) = self.referent(owner) {
            // Implementations for the same reference kind of `T` may sit in other modules,
            // each with its own owner identity; all of them are the receiver type's methods.
            owners.extend(
                self.referents
                    .values()
                    .filter(|(other, other_referent)| {
                        other_referent.file.path == referent.file.path
                            && other_referent.symbol.item_range == referent.symbol.item_range
                            && other.symbol.name.starts_with("&mut") == unique
                    })
                    .map(|(other, _)| (*other, true)),
            );
            owners.push((referent, false));
        } else {
            owners.push((owner, true));
        }
        let mut visited = std::collections::BTreeSet::new();
        for (owner, is_written) in owners {
            let Some(mut path) = owner.admitted_path() else {
                continue;
            };
            if !visited.insert(path.clone()) {
                continue;
            }
            path.push(member.to_owned());
            for method in self.methods.get(&path).into_iter().flatten() {
                if !work.charge() || !self.available(method.target) {
                    return None;
                }
                let mut references = match self_kind(method.target)? {
                    SelfKind::Value => Vec::new(),
                    SelfKind::Shared => vec![Reference::Shared],
                    SelfKind::Unique => vec![Reference::Unique],
                    SelfKind::Other => return None,
                    SelfKind::Absent => continue,
                };
                if is_written {
                    references.extend(written.iter().copied());
                }
                candidates.push(Candidate {
                    receiver: Probed {
                        references,
                        base: 0,
                    },
                    inherent: !trait_method(method)?,
                    method: Some(*method),
                });
            }
        }
        Some(())
    }

    /// The written type of a structural owner, then the types it dereferences to.
    fn written_bases<'index>(
        &'index self,
        mut file: &'index File<'source>,
        owner: Target<'source>,
        work: &mut Work,
    ) -> Option<Vec<Base>> {
        // Only a tuple is an owner outside its own file, and it has no inherent method.
        if owner.file.path != file.document.path {
            return Some(Vec::new());
        }
        let mut range = owner.symbol.name_range?;
        let mut written = file.text(range)?.to_owned();
        let mut bases = Vec::new();
        // References an alias is written behind.
        let mut before = Vec::new();
        for _ in 0..BASES_MAX {
            let (mut references, inner) = split_references(&written);
            references.splice(0..0, before.drain(..));
            if let Some(alias) = self.type_alias(file, range.start, inner, work) {
                if alias.file.path != file.document.path {
                    file = self.source_files.get(alias.file.path.as_str())?;
                }
                range = file
                    .declared_types
                    .get(&alias.symbol.name_range?)
                    .copied()
                    .flatten()?;
                before = references;
                written = file.text(range)?.to_owned();
                continue;
            }
            let names = self.standard_names(file, range.start, inner, work)?;
            let next = dereferenced(&names, inner);
            bases.push(Base { references, names });
            let Some(next) = next else {
                break;
            };
            written = next;
        }
        Some(bases)
    }

    /// The type alias named by a written path.
    fn type_alias(
        &self,
        file: &File<'source>,
        position: u64,
        written: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let parts = path_segments(written, work)?;
        let path = self.resolved_path(file, position, &parts, work)?;
        self.lookup(file, position, &path, Namespace::Type, work)
            .filter(|alias| alias.symbol.kind == "type_alias")
    }

    /// The names under which the standard library declares the methods of a written type;
    /// none for a type it does not declare. `None` when the work ended.
    fn standard_names(
        &self,
        file: &File<'source>,
        position: u64,
        written: &str,
        work: &mut Work,
    ) -> Option<Vec<String>> {
        if work.exhausted {
            return None;
        }
        if let Some(inner) = bracketed(written) {
            let (element, array) = match inner.1 {
                Some(element) => (element, true),
                None => (inner.0, false),
            };
            let kind = if array { "array" } else { "slice" };
            let element = if bracketed(element).is_some_and(|inner| inner.1.is_some()) {
                "array"
            } else {
                let path = element.split('<').next().unwrap_or_default().trim();
                path.rsplit("::").next().unwrap_or_default()
            };
            return Some(vec![kind.to_owned(), format!("{kind}:{element}")]);
        }
        if written.starts_with("*const ") || written.starts_with("*mut ") {
            return Some(vec!["pointer".to_owned()]);
        }
        let name = self.standard_name(file, position, written, work);
        (!work.exhausted).then(|| name.into_iter().collect())
    }

    /// The path below the root of the standard library that a written type path names.
    fn standard_name(
        &self,
        file: &File<'source>,
        position: u64,
        written: &str,
        work: &mut Work,
    ) -> Option<String> {
        let written = written.strip_prefix("::").unwrap_or(written);
        let parts = path_segments(written, work)?;
        let (head, tail) = parts.split_first()?;
        let mut path = match file.binding(head, position, is_type_binding, work) {
            Binding::Found(binding) => {
                let StaticBindingKind::RustUse { path } = binding.kind else {
                    return None;
                };
                let mut imported = file.use_parts(path, work)?;
                imported.extend(tail.iter().map(|part| (*part).to_owned()));
                imported
            }
            Binding::Missing | Binding::Blocked if tail.is_empty() => {
                let mut local = self.module(file, position, work)?.to_vec();
                local.push((*head).to_owned());
                if self.types.contains_key(&local) {
                    return None;
                }
                let name = match rust_prelude(head) {
                    Some(path) => path.split_once("::")?.1,
                    None => head,
                };
                return Some(name.to_owned());
            }
            Binding::Missing => parts.iter().map(|part| (*part).to_owned()).collect(),
            Binding::Blocked => return None,
        };
        // A module of the package can forward the names of the standard library. Each step
        // follows one binding of a module, so a cycle ends at the bound.
        for _ in 0..RECEIVER_DEPTH_MAX {
            if !work.charge() {
                return None;
            }
            let head = path.first()?;
            if RUST_RUNTIME_CRATES.contains(&head.as_str()) {
                return Some(path.get(1..)?.join("::"));
            }
            let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
            let module = self.module(file, position, work)?.first()?;
            let local = if head == module {
                path.clone()
            } else {
                self.absolute_path(file, position, &parts, work)?
            };
            path.clone_from(&self.exports.get(&local)?.as_ref()?.target);
        }
        None
    }
}

/// The references in front of the owner in the type of a written receiver: the form of its
/// `self` parameter, or what its declaration writes before the type's name.
fn receiver_references(
    file: &File<'_>,
    mut receiver: ByteRange,
    work: &mut Work,
) -> Option<Vec<Reference>> {
    for _ in 0..RECEIVER_DEPTH_MAX {
        if !work.charge() {
            return None;
        }
        let name = file.text(receiver)?.trim();
        let Binding::Found(binding) = file.binding(name, receiver.start, is_value_binding, work)
        else {
            return None;
        };
        match binding.kind {
            StaticBindingKind::RustValue { initializer } => receiver = initializer,
            StaticBindingKind::RustReceiver { .. } if name == "self" => {
                return match receiver_form(file.document.source, binding)? {
                    SelfKind::Value => Some(Vec::new()),
                    SelfKind::Shared => Some(vec![Reference::Shared]),
                    SelfKind::Unique => Some(vec![Reference::Unique]),
                    SelfKind::Other | SelfKind::Absent => None,
                };
            }
            StaticBindingKind::RustReceiver { type_path } => {
                let before = file.text(ByteRange {
                    start: binding.name.end,
                    end: type_path.start,
                })?;
                let (references, rest) = split_references(before.trim_start().strip_prefix(':')?);
                return rest.is_empty().then_some(references);
            }
            _ => return None,
        }
    }
    None
}

/// The method the first probe with a candidate takes.
fn probe<'source>(
    file: &File<'source>,
    position: u64,
    probed: &[Probed],
    candidates: &[Candidate<'source>],
) -> Option<Method<'source>> {
    for probed in probed {
        for borrowed in [None, Some(Reference::Shared), Some(Reference::Unique)] {
            let taken = candidates
                .iter()
                .filter(|candidate| {
                    candidate.receiver.base == probed.base
                        && candidate
                            .receiver
                            .references
                            .iter()
                            .eq(borrowed.iter().chain(&probed.references))
                })
                .collect::<Vec<_>>();
            if taken.is_empty() {
                continue;
            }
            let of = |inherent: bool| {
                taken
                    .iter()
                    .filter(move |candidate| candidate.inherent == inherent)
                    .filter_map(|candidate| candidate.method)
                    .collect::<Vec<_>>()
            };
            let standard = taken.iter().any(|candidate| candidate.method.is_none());
            return match (of(true).as_slice(), of(false).as_slice()) {
                ([], _) if standard => None,
                ([method], _) | ([], [method]) => Some(*method),
                ([], methods) => enclosing_method(file, position, methods),
                _ => None,
            };
        }
    }
    None
}

/// The method of the implementation around `position`. The trait of that implementation is in
/// scope there, and compiled code has no second candidate of the name.
pub(super) fn enclosing_method<'source>(
    file: &File<'source>,
    position: u64,
    methods: &[Method<'source>],
) -> Option<Method<'source>> {
    let mut enclosing = methods.iter().filter(|method| {
        method.target.file.path == file.document.path
            && method.implementation.range.contains(position)
    });
    let first = enclosing.next()?;
    enclosing.next().is_none().then_some(*first)
}

/// The inside of a written slice or array type, and the element type of an array.
fn bracketed(written: &str) -> Option<(&str, Option<&str>)> {
    let inner = written.strip_prefix('[')?.strip_suffix(']')?.trim();
    let mut depth = 0_usize;
    for (index, byte) in inner.bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' | b'<' => depth = depth.checked_add(1)?,
            b'>' if index > 0 && inner.as_bytes()[index - 1] == b'-' => {}
            b')' | b']' | b'}' | b'>' => depth = depth.checked_sub(1)?,
            b';' if depth == 0 => return Some((inner, Some(inner[..index].trim()))),
            _ => {}
        }
    }
    Some((inner, None))
}

/// The type a written standard type dereferences to; an array unsizes to its slice.
fn dereferenced(names: &[String], written: &str) -> Option<String> {
    match names.first()?.as_str() {
        "array" => Some(format!("[{}]", bracketed(written)?.1?)),
        "vec::Vec" => Some(format!("[{}]", first_type_argument(written)?)),
        "string::String" => Some("str".to_owned()),
        "path::PathBuf" => Some("std::path::Path".to_owned()),
        "ffi::OsString" => Some("std::ffi::OsStr".to_owned()),
        "ffi::CString" => Some("std::ffi::CStr".to_owned()),
        name if POINTERS.contains(&name) => first_type_argument(written).map(str::to_owned),
        _ => None,
    }
}

/// The first generic argument of a written path that is no lifetime.
fn first_type_argument(written: &str) -> Option<&str> {
    let arguments = written.strip_suffix('>')?.split_once('<')?.1;
    split_top_level(arguments)?
        .into_iter()
        .map(str::trim)
        .find(|argument| !argument.starts_with('\''))
}

/// The references in front of a written type, and the type without them.
pub(super) fn split_references(mut written: &str) -> (Vec<Reference>, &str) {
    let mut references = Vec::new();
    // Each step removes at least one byte of the bounded type text.
    loop {
        let trimmed = written.trim();
        let Some(rest) = trimmed.strip_prefix('&') else {
            return (references, trimmed);
        };
        let mut rest = rest.trim_start();
        if let Some(lifetime) = rest.strip_prefix('\'') {
            rest = lifetime.trim_start_matches(|character: char| {
                character.is_alphanumeric() || character == '_'
            });
        }
        let rest = rest.trim_start();
        let unique = rest
            .strip_prefix("mut")
            .filter(|after| after.is_empty() || after.starts_with(char::is_whitespace));
        references.push(if unique.is_some() {
            Reference::Unique
        } else {
            Reference::Shared
        });
        written = unique.unwrap_or(rest);
    }
}
