//! Python loop targets typed by the element type written for the iterated container.

use std::collections::BTreeSet;

use rift_syntax::{ByteRange, StaticBindingKind};

use super::super::{File, Index, Target, Work};
use super::annotations::{slice_range, top_level_members};
use super::{Value, edge_python_binding};

/// Builtin containers whose single type argument is the element type.
const BUILTIN_CONTAINERS: [&str; 3] = ["list", "set", "frozenset"];

/// `typing` containers whose single type argument is the element type.
const TYPING_CONTAINERS: [&str; 11] = [
    "List",
    "Set",
    "FrozenSet",
    "Sequence",
    "MutableSequence",
    "Iterable",
    "Iterator",
    "Collection",
    "AbstractSet",
    "MutableSet",
    "Deque",
];

impl<'source> Index<'source> {
    /// The class of each element a loop over `iterable` binds: the element type written for a
    /// called function's return, for an annotated name, or for a member declared by annotation or
    /// by an unchanged alias of an annotated name.
    pub(super) fn python_element(
        &self,
        file: &File<'source>,
        iterable: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if let Some(call) = file
            .python_calls
            .get(&iterable.start)
            .into_iter()
            .flatten()
            .find(|call| call.range == iterable)
        {
            let callee = self.edge_python_call(file, call, work)?;
            let source = self.files.get(callee.file.path.as_str())?;
            let StaticBindingKind::PythonFunction { annotation, .. } =
                *source.python_values.get(&callee.symbol.name_range?.end)?
            else {
                return None;
            };
            return self.python_element_annotation(source, annotation, work);
        }
        let text = file.text(iterable)?.trim();
        if !text.contains('.') {
            return self.python_named_element(file, text, iterable.start, work);
        }
        let declaration = match work.python_lookup(|work| self.python_name(file, iterable, work))? {
            Value::Symbol(target)
            | Value::Member {
                declaration: target,
                ..
            } => target,
            Value::Instance(_) | Value::Module(_) => return None,
        };
        self.python_declared_element(declaration, work)
    }

    fn python_named_element(
        &self,
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let binding = edge_python_binding(self, file, name, position, work)?;
        if let StaticBindingKind::PythonAnnotation { annotation } = binding.kind {
            return self.python_element_annotation(file, annotation, work);
        }
        self.python_declared_element(file.target_at(binding.name)?, work)
    }

    /// Follows a declaration through unchanged aliases to the annotation written for it.
    fn python_declared_element(
        &self,
        mut declaration: Target<'source>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut visited = BTreeSet::new();
        loop {
            let name = declaration.symbol.name_range?;
            if !work.charge() || !visited.insert((declaration.file.path.as_str(), name.start)) {
                return None;
            }
            let file = self.files.get(declaration.file.path.as_str())?;
            match *file.python_values.get(&name.end)? {
                StaticBindingKind::PythonAnnotation { annotation } => {
                    return self.python_element_annotation(file, annotation, work);
                }
                StaticBindingKind::PythonAlias { value } => {
                    let text = file.text(value)?.trim();
                    if !text.contains('.') {
                        return self.python_named_element(file, text, value.start, work);
                    }
                    declaration =
                        match work.python_lookup(|work| self.python_name(file, value, work))? {
                            Value::Symbol(target)
                            | Value::Member {
                                declaration: target,
                                ..
                            } => target,
                            Value::Instance(_) | Value::Module(_) => return None,
                        };
                }
                _ => return None,
            }
        }
    }

    /// The element class of a container annotation with one type argument, also inside
    /// `Optional[...]`.
    fn python_element_annotation(
        &self,
        file: &File<'source>,
        annotation: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if !work.charge() {
            return None;
        }
        let written = file.text(annotation)?;
        let text = written.trim().trim_matches(['\'', '"']);
        if let Some(inner) = self.python_optional_member(file, annotation, written, text, work) {
            return self.python_element_annotation(file, inner, work);
        }
        let open = text.find('[')?;
        let inner = text.strip_suffix(']')?.get(open + 1..)?;
        let head_text = text.get(..open)?.trim();
        let head = slice_range(annotation, written, head_text)?;
        let container = if BUILTIN_CONTAINERS.contains(&head_text) {
            !file.by_name.contains_key(head_text) && !file.blocked(annotation.start, work)
        } else {
            TYPING_CONTAINERS
                .iter()
                .any(|name| self.python_typing_name(file, head, name, work))
        };
        if !container {
            return None;
        }
        let [member] = top_level_members(inner)[..] else {
            return None;
        };
        self.python_annotation(file, slice_range(annotation, written, member.trim())?, work)
    }
}
