//! Names a module lists in `__all__`.

use std::collections::{BTreeMap, VecDeque};

use rift_error::RiftError;
use tree_sitter::Node;

use super::PythonKinds;
use crate::extract::byte_range;
use crate::{ByteRange, SyntaxExportBinding, SyntaxExportKind};

/// The module variable listing the names a module exports.
const DECLARED_EXPORTS_NAME: &str = "__all__";
/// Builtin functions that read a list they receive and keep no reference to it.
const READING_BUILTINS: [&str; 10] = [
    "any",
    "dict",
    "frozenset",
    "len",
    "list",
    "print",
    "repr",
    "set",
    "sorted",
    "tuple",
];

/// Ancestors a mention is classified by: its attribute, that attribute's call and statement.
const ANCESTORS: usize = 4;

/// One identifier spelled `__all__` with the elements around it, the nearest first.
struct Mention<'tree> {
    name: Node<'tree>,
    ancestors: [Option<Node<'tree>>; ANCESTORS],
}

/// The listed names after every statement that writes the list, each with its statement.
#[derive(Default)]
struct DeclaredExports<'tree, 'text> {
    /// The assignment that created the current list and the name it assigned.
    definition: Option<(Node<'tree>, Node<'tree>)>,
    /// Entries in order; a removed entry is absent.
    names: Vec<Option<(Node<'tree>, Node<'tree>)>>,
    /// Places of the entries that spell each name, in order.
    places: BTreeMap<&'text str, VecDeque<usize>>,
}

/// The export bindings a module's `__all__` states: one for the assigned name, then one for
/// each listed name.
///
/// A statement directly in the module body may assign a list or tuple of plain string
/// literals, extend it with `+=` or `extend`, or change it with `append` or `remove`. Any other
/// write, and any use that may share the list, leaves it unknown and records nothing.
pub(super) fn declared_exports(
    kinds: &PythonKinds,
    root: Node<'_>,
    text: &str,
) -> Result<Vec<SyntaxExportBinding>, RiftError> {
    if !text.contains(DECLARED_EXPORTS_NAME) {
        return Ok(Vec::new());
    }
    let mut declared = DeclaredExports::default();
    for mention in mentions(kinds, root, text) {
        if !kinds.apply_mention(&mention, root, text, &mut declared) {
            return Ok(Vec::new());
        }
    }
    let Some((statement, name)) = declared.definition else {
        return Ok(Vec::new());
    };
    let mut bindings = vec![binding(statement, byte_range(name)?)?];
    for (statement, content) in declared.names.into_iter().flatten() {
        bindings.push(binding(statement, byte_range(content)?)?);
    }
    Ok(bindings)
}

fn binding(statement: Node<'_>, name: ByteRange) -> Result<SyntaxExportBinding, RiftError> {
    Ok(SyntaxExportBinding {
        kind: SyntaxExportKind::Named,
        range: byte_range(statement)?,
        local: Some(name),
        exported: Some(name),
        source: None,
        container: None,
        type_only: false,
    })
}

/// Every identifier spelled `__all__`, in source order. Reads each accepted syntax element once
/// and keeps the elements around the current one.
fn mentions<'tree>(kinds: &PythonKinds, root: Node<'tree>, text: &str) -> Vec<Mention<'tree>> {
    let mut found = Vec::new();
    let mut around = Vec::new();
    let mut cursor = root.walk();
    'walk: loop {
        let element = cursor.node();
        if element.kind_id() == kinds.identifier
            && text.get(element.byte_range()) == Some(DECLARED_EXPORTS_NAME)
        {
            let mut ancestors = [None; ANCESTORS];
            for (ancestor, held) in ancestors.iter_mut().zip(around.iter().rev()) {
                *ancestor = Some(*held);
            }
            found.push(Mention {
                name: element,
                ancestors,
            });
        }
        if cursor.goto_first_child() {
            around.push(element);
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
            around.pop();
        }
    }
    found
}

impl PythonKinds {
    /// Applies one mention of `__all__`; `false` when the list it leaves is unknown.
    fn apply_mention<'tree, 'text>(
        &self,
        mention: &Mention<'tree>,
        root: Node<'tree>,
        text: &'text str,
        declared: &mut DeclaredExports<'tree, 'text>,
    ) -> bool {
        let name = mention.name;
        let [Some(parent), rest @ ..] = mention.ancestors else {
            return false;
        };
        let id = parent.kind_id();
        let left = parent.child_by_field_id(self.left.get()) == Some(name);
        if id == self.assignment && left {
            let Some(value) = parent.child_by_field_id(self.right.get()) else {
                return true;
            };
            return self
                .module_statement(&rest, root)
                .zip(self.literal_names(value, text))
                .is_some_and(|(statement, names)| {
                    declared.assign(statement, name, names);
                    true
                });
        }
        if id == self.augmented_assignment && left {
            let added = parent
                .child_by_field_id(self.operator.get())
                .is_some_and(|operator| text.get(operator.byte_range()) == Some("+="));
            return added
                && self
                    .module_statement(&rest, root)
                    .zip(parent.child_by_field_id(self.right.get()))
                    .and_then(|(statement, value)| {
                        Some((statement, self.literal_names(value, text)?))
                    })
                    .is_some_and(|(statement, names)| declared.extend(statement, names));
        }
        if id == self.attribute {
            // `module.__all__` reads or writes another module's list.
            return parent.child_by_field_id(self.object.get()) != Some(name)
                || self.apply_method(parent, &rest, root, text, declared);
        }
        self.reads(name, parent, rest[0], text)
    }

    /// Whether `name` stands where an expression reads its value without sharing the list.
    fn reads(
        &self,
        name: Node<'_>,
        parent: Node<'_>,
        holder: Option<Node<'_>>,
        text: &str,
    ) -> bool {
        let id = parent.kind_id();
        if self.loop_kinds.contains(&id) {
            return parent.child_by_field_id(self.right.get()) == Some(name);
        }
        if id == self.argument_list {
            return holder
                .and_then(|call| call.child_by_field_id(self.function_field.get()))
                .and_then(|function| text.get(function.byte_range()))
                .is_some_and(|function| READING_BUILTINS.contains(&function));
        }
        self.reading_kinds.contains(&id)
    }

    /// Applies `__all__.extend([..])`, `__all__.append("..")` or `__all__.remove("..")`.
    fn apply_method<'tree, 'text>(
        &self,
        attribute: Node<'tree>,
        around: &[Option<Node<'tree>>; ANCESTORS - 1],
        root: Node<'tree>,
        text: &'text str,
        declared: &mut DeclaredExports<'tree, 'text>,
    ) -> bool {
        let [Some(call), rest @ ..] = *around else {
            return false;
        };
        if call.kind_id() != self.call
            || call.child_by_field_id(self.function_field.get()) != Some(attribute)
        {
            return false;
        }
        let Some(statement) = self.module_statement(&rest, root) else {
            return false;
        };
        let Some(arguments) = call.child_by_field_id(self.arguments.get()) else {
            return false;
        };
        let mut cursor = arguments.walk();
        let mut values = arguments
            .named_children(&mut cursor)
            .filter(|argument| !argument.is_extra());
        let (Some(value), None) = (values.next(), values.next()) else {
            return false;
        };
        let method = attribute
            .child_by_field_id(self.attribute_name.get())
            .and_then(|method| text.get(method.byte_range()));
        match method {
            Some("extend") => self
                .literal_names(value, text)
                .is_some_and(|names| declared.extend(statement, names)),
            Some("append") => self
                .literal_name(value, text)
                .is_some_and(|name| declared.extend(statement, vec![name])),
            Some("remove") => self
                .literal_name(value, text)
                .is_some_and(|(_, name)| declared.remove(name)),
            _ => false,
        }
    }

    /// The expression statement of the module body that directly holds the expression whose
    /// ancestors are `around`.
    fn module_statement<'tree>(
        &self,
        around: &[Option<Node<'tree>>],
        root: Node<'tree>,
    ) -> Option<Node<'tree>> {
        let &[Some(statement), Some(body), ..] = around else {
            return None;
        };
        (statement.kind_id() == self.expression_statement && body == root).then_some(statement)
    }

    /// The string contents of a list or tuple that holds plain string literals alone.
    fn literal_names<'tree, 'text>(
        &self,
        value: Node<'tree>,
        text: &'text str,
    ) -> Option<Vec<(Node<'tree>, &'text str)>> {
        if !self.sequence_kinds.contains(&value.kind_id()) {
            return None;
        }
        let mut cursor = value.walk();
        value
            .named_children(&mut cursor)
            .filter(|element| !element.is_extra())
            .map(|element| self.literal_name(element, text))
            .collect()
    }

    /// The content of a string literal without escapes, interpolation or a prefix.
    fn literal_name<'tree, 'text>(
        &self,
        value: Node<'tree>,
        text: &'text str,
    ) -> Option<(Node<'tree>, &'text str)> {
        if value.kind_id() != self.string {
            return None;
        }
        let mut cursor = value.walk();
        let mut content = None;
        for part in value.named_children(&mut cursor) {
            if part.kind_id() == self.string_content {
                if part.named_child_count() > 0 || content.is_some() {
                    return None;
                }
                content = Some(part);
            } else if part.kind_id() == self.string_start
                && !text
                    .get(part.byte_range())?
                    .starts_with(['"', '\'', 'u', 'U', 'r', 'R'])
            {
                return None;
            }
        }
        let content = content?;
        Some((content, text.get(content.byte_range())?))
    }
}

impl<'tree, 'text> DeclaredExports<'tree, 'text> {
    fn assign(
        &mut self,
        statement: Node<'tree>,
        name: Node<'tree>,
        names: Vec<(Node<'tree>, &'text str)>,
    ) {
        self.names.clear();
        self.places.clear();
        self.definition = Some((statement, name));
        self.extend(statement, names);
    }

    /// Adds names to a list an earlier statement created.
    fn extend(&mut self, statement: Node<'tree>, names: Vec<(Node<'tree>, &'text str)>) -> bool {
        for (content, name) in names {
            self.places
                .entry(name)
                .or_default()
                .push_back(self.names.len());
            self.names.push(Some((statement, content)));
        }
        self.definition.is_some()
    }

    /// Removes the first listed name equal to `name`, as `list.remove` does.
    fn remove(&mut self, name: &str) -> bool {
        let Some(place) = self.places.get_mut(name).and_then(VecDeque::pop_front) else {
            return false;
        };
        self.names[place] = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use crate::{SyntaxFacts, SyntaxLimits, SyntaxProvider, SyntaxSource};

    /// The names the export bindings of `text` spell, the assigned `__all__` first.
    fn listed(text: &str) -> Vec<String> {
        let path = ProjectPath::new("mod.py").expect("path");
        let document = crate::PythonSyntaxProvider::default()
            .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
            .expect("source");
        let facts = document.facts();
        assert_retained(text, facts);
        facts
            .export_bindings()
            .unwrap_or_default()
            .iter()
            .filter(|binding| binding.source.is_none())
            .map(|binding| {
                let local = binding.local.expect("listed name");
                assert_eq!(binding.exported, Some(local));
                let start = usize::try_from(local.start).expect("start");
                let end = usize::try_from(local.end).expect("end");
                text[start..end].to_owned()
            })
            .collect()
    }

    fn assert_retained(text: &str, facts: &SyntaxFacts) {
        for binding in facts.export_bindings().unwrap_or_default() {
            let local = binding.local.expect("listed name");
            assert!(binding.range.start <= local.start && local.end <= binding.range.end);
            assert!(usize::try_from(binding.range.end).expect("end") <= text.len());
        }
        let starts = facts
            .export_bindings()
            .unwrap_or_default()
            .iter()
            .map(|binding| binding.range.start)
            .collect::<Vec<_>>();
        assert!(starts.is_sorted());
    }

    #[test]
    fn literal_lists_and_their_changes_record_each_listed_name() {
        for (text, expected) in [
            ("__all__ = [\"a\", 'b']\n", vec!["a", "b"]),
            (
                "__all__ = (\n    \"a\",  # first\n    \"b\",\n)\n",
                vec!["a", "b"],
            ),
            ("__all__ = \"a\", \"b\"\n", vec!["a", "b"]),
            ("__all__: list[str] = [\"a\"]\n", vec!["a"]),
            ("__all__: list[str]\n__all__ = [\"a\"]\n", vec!["a"]),
            ("__all__ = []\n", vec![]),
            ("__all__ = [\"a\"]\n__all__ += [\"b\"]\n", vec!["a", "b"]),
            (
                "__all__ = [\"a\"]\n__all__.extend((\"b\",))\n__all__.append(\"c\")\n",
                vec!["a", "b", "c"],
            ),
            (
                "__all__ = [\"a\", \"b\"]\n__all__.remove(\"a\")\n",
                vec!["b"],
            ),
            ("__all__ = [\"a\"]\n__all__ = [\"b\"]\n", vec!["b"]),
            (
                "from .m import Name as Name\n__all__ = [\"a\"]\nfrom .n import Other as Other\n",
                vec!["a"],
            ),
            (
                "import other\n__all__ = [\"a\"]\nother.__all__ = []\ndef names(): return __all__\nfor name in __all__:\n    print(__all__, name in __all__)\ncount = len(__all__)\nmore = __all__ + other.__all__\n",
                vec!["a"],
            ),
            (
                "__all__ = [\"a\", \"b\", \"a\"]\n__all__.remove(\"a\")\n__all__.remove(\"a\")\n",
                vec!["b"],
            ),
        ] {
            let mut names = vec!["__all__".to_owned()];
            names.extend(expected.into_iter().map(str::to_owned));
            assert_eq!(listed(text), names, "{text}");
        }
    }

    #[test]
    fn lists_a_module_computes_stay_unknown() {
        for text in [
            "def run(): pass\n",
            "__all__ = names()\n",
            "__all__ = [\"a\", name]\n",
            "__all__ = [\"a\", *others]\n",
            "__all__ = [f\"a\"]\n",
            "__all__ = [b\"a\"]\n",
            "__all__ = [\"a\\n\"]\n",
            "__all__ = [\"a\"] + other.__all__\n",
            "__all__ = other = [\"a\"]\n",
            "__all__ += [\"a\"]\n",
            "__all__ = [\"a\"]\n__all__ += other.__all__\n",
            "__all__ = [\"a\"]\n__all__ *= 2\n",
            "__all__ = [\"a\"]\n__all__.extend(names)\n",
            "__all__ = [\"a\"]\n__all__.append(\"b\", \"c\")\n",
            "__all__ = [\"a\"]\n__all__.remove(\"b\")\n",
            "__all__ = [\"a\"]\n__all__.sort()\n",
            "__all__ = [\"a\"]\nextend = __all__.extend\n",
            "__all__ = [\"a\"]\n__all__[0] = \"b\"\n",
            "__all__ = [\"a\"]\ndel __all__\n",
            "__all__ = [\"a\"]\nif flag:\n    __all__ += [\"b\"]\n",
            "__all__ = [\"a\"]\ndef change():\n    global __all__\n    __all__ = []\n",
            "__all__ = [\"a\"]\ndef change():\n    __all__.append(\"b\")\n",
            "__all__ = [\"a\"]\nfrom .other import __all__\n",
            "__all__ = [\"a\"]\nfor __all__ in lists: pass\n",
            "__all__, other = [\"a\"], 1\n",
            "__all__ = [\"a\"]\nother = __all__\n",
            "__all__ = [\"a\"]\nregister(__all__)\n",
            "__all__ = [\"a\"]\nregister(names=__all__)\n",
            "__all__ = [\"a\"]\nother = (__all__, 1)\n",
            "__all__ = [\"a\"]\nother = __all__ or []\n",
        ] {
            assert_eq!(listed(text), Vec::<String>::new(), "{text}");
        }
    }
}
