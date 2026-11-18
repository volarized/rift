//! Angular component ownership from captured TypeScript syntax and import evidence.

use std::collections::{BTreeMap, BTreeSet};

use crate::{ByteRange, SyntaxDocument, SyntaxElement, SyntaxSource};

const ANGULAR_CORE: &str = "@angular/core";
const COMPONENT: &str = "Component";
const TEMPLATE: &str = "template";
const TEMPLATE_URL: &str = "templateUrl";

/// One component decorator imported from Angular's core package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AngularComponent {
    /// Complete class range in the original source.
    pub range: ByteRange,
    /// Template properties carried by the decorator's configuration object.
    pub templates: Vec<AngularTemplate>,
}

/// One Angular component's authored template property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AngularTemplate {
    /// Original bytes between a literal template's quote delimiters.
    Inline {
        /// UTF-8 byte range into the TypeScript component source.
        range: ByteRange,
    },
    /// Literal template path relative to its component source file.
    External {
        /// Authored path; workspace and package analysis resolve ownership.
        path: String,
    },
    /// A dynamic or escaped value whose runtime bytes cannot be established from syntax.
    Unresolved {
        /// Original expression range preserved for context warnings.
        range: ByteRange,
    },
}

/// Finds Angular components from their imports and captured TypeScript declaration syntax.
///
/// Work follows the bounded document's syntax elements and ancestor depth. This function performs
/// no parsing and preserves dynamic template expressions as unresolved source ranges.
#[must_use]
pub fn angular_components(
    source: SyntaxSource<'_>,
    document: &SyntaxDocument,
) -> Vec<AngularComponent> {
    let children = Children::new(document.syntax_elements());
    let imports = component_imports(source.text, &children);
    let shadows = binding_shadows(source.text, &children, &imports);
    document
        .syntax_elements()
        .iter()
        .enumerate()
        .filter(|(_, syntax_element)| syntax_element.kind == "decorator")
        .filter_map(|(index, _)| component(source.text, &children, &imports, &shadows, index))
        .collect()
}

pub(crate) struct Children<'syntax_elements> {
    pub(crate) syntax_elements: &'syntax_elements [SyntaxElement],
    first: Vec<Option<usize>>,
    next: Vec<Option<usize>>,
}

impl<'syntax_elements> Children<'syntax_elements> {
    pub(crate) fn new(syntax_elements: &'syntax_elements [SyntaxElement]) -> Self {
        let mut first = vec![None; syntax_elements.len()];
        let mut next = vec![None; syntax_elements.len()];
        let mut last = vec![None; syntax_elements.len()];
        for (index, syntax_element) in syntax_elements.iter().enumerate() {
            if let Some(parent) = syntax_element.parent {
                if let Some(previous) = last[parent] {
                    next[previous] = Some(index);
                } else {
                    first[parent] = Some(index);
                }
                last[parent] = Some(index);
            }
        }
        Self {
            syntax_elements,
            first,
            next,
        }
    }

    pub(crate) fn indices(&self, parent: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(self.first[parent], |index| self.next[*index])
    }

    pub(crate) fn child(&self, parent: usize, kind: &str) -> Option<usize> {
        self.indices(parent)
            .find(|index| self.syntax_elements[*index].kind == kind)
    }

    pub(crate) fn text<'source>(&self, index: usize, text: &'source str) -> Option<&'source str> {
        let range = self.syntax_elements[index].range;
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        text.get(start..end)
    }

    pub(crate) fn literal<'source>(
        &self,
        index: usize,
        text: &'source str,
    ) -> Option<(&'source str, ByteRange)> {
        let syntax_element = &self.syntax_elements[index];
        if !matches!(syntax_element.kind, "string" | "template_string") {
            return None;
        }
        if self.indices(index).any(|child| {
            matches!(
                self.syntax_elements[child].kind,
                "escape_sequence" | "template_substitution"
            )
        }) {
            return None;
        }
        let written = self.text(index, text)?;
        let quote = *written.as_bytes().first()?;
        if !matches!(quote, b'\'' | b'"' | b'`') || written.as_bytes().last() != Some(&quote) {
            return None;
        }
        let range = ByteRange {
            start: syntax_element.range.start + 1,
            end: syntax_element.range.end.checked_sub(1)?,
        };
        written
            .get(1..written.len().checked_sub(1)?)
            .map(|value| (value, range))
    }
}

fn component_imports(text: &str, children: &Children<'_>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (index, syntax_element) in children.syntax_elements.iter().enumerate() {
        if syntax_element.kind != "import_statement" {
            continue;
        }
        let Some(source) = children.child(index, "string") else {
            continue;
        };
        if children.literal(source, text).map(|(value, _)| value) != Some(ANGULAR_CORE) {
            continue;
        }
        let Some(clause) = children.child(index, "import_clause") else {
            continue;
        };
        if type_modifier(
            text,
            children,
            index,
            children.syntax_elements[clause].range.start,
        ) {
            continue;
        }
        import_clause(text, children, clause, &mut names);
    }
    names
}

fn type_modifier(text: &str, children: &Children<'_>, index: usize, end: u64) -> bool {
    let mut start = children.syntax_elements[index].range.start;
    for child in children.indices(index).filter(|child| {
        children.syntax_elements[*child].kind == "comment"
            && children.syntax_elements[*child].range.start < end
    }) {
        let range = children.syntax_elements[child].range;
        if modifier_in(text, start, range.start) {
            return true;
        }
        start = range.end;
    }
    modifier_in(text, start, end)
}

fn modifier_in(text: &str, start: u64, end: u64) -> bool {
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return false;
    };
    text.get(start..end).is_some_and(|prefix| {
        prefix
            .split_ascii_whitespace()
            .any(|word| matches!(word, "type" | "typeof"))
    })
}

fn import_clause(text: &str, children: &Children<'_>, clause: usize, names: &mut BTreeSet<String>) {
    for index in children.indices(clause) {
        match children.syntax_elements[index].kind {
            "namespace_import" => {
                if let Some(name) = children
                    .child(index, "identifier")
                    .and_then(|index| children.text(index, text))
                {
                    names.insert(format!("{name}.{COMPONENT}"));
                }
            }
            "named_imports" => named_imports(text, children, index, names),
            _ => {}
        }
    }
}

fn named_imports(
    text: &str,
    children: &Children<'_>,
    imports: usize,
    names: &mut BTreeSet<String>,
) {
    for index in children.indices(imports) {
        if children.syntax_elements[index].kind != "import_specifier"
            || children.child(index, "type").is_some()
        {
            continue;
        }
        let mut identifiers = children
            .indices(index)
            .filter(|index| children.syntax_elements[*index].kind == "identifier");
        let Some(name) = identifiers
            .next()
            .and_then(|index| children.text(index, text))
        else {
            continue;
        };
        let Some(first) = children.child(index, "identifier") else {
            continue;
        };
        if type_modifier(
            text,
            children,
            index,
            children.syntax_elements[first].range.start,
        ) {
            continue;
        }
        if name != COMPONENT {
            continue;
        }
        let local = identifiers
            .next()
            .and_then(|index| children.text(index, text))
            .unwrap_or(name);
        names.insert(local.to_owned());
    }
}

fn component(
    text: &str,
    children: &Children<'_>,
    imports: &BTreeSet<String>,
    shadows: &BTreeMap<usize, BTreeSet<String>>,
    decorator: usize,
) -> Option<AngularComponent> {
    let parent = children.syntax_elements[decorator].parent?;
    let class = if children.syntax_elements[parent].kind == "export_statement" {
        children.child(parent, "class_declaration")?
    } else {
        parent
    };
    if children.syntax_elements[class].kind != "class_declaration" {
        return None;
    }
    let call = children.child(decorator, "call_expression")?;
    let name = children.indices(call).find(|index| {
        matches!(
            children.syntax_elements[*index].kind,
            "identifier" | "member_expression"
        )
    })?;
    let callee = if children.syntax_elements[name].kind == "member_expression" {
        let object = children.child(name, "identifier")?;
        let property = children.child(name, "property_identifier")?;
        format!(
            "{}.{}",
            children.text(object, text)?,
            children.text(property, text)?
        )
    } else {
        children.text(name, text)?.to_owned()
    };
    if !imports.contains(&callee) {
        return None;
    }
    let root = callee.split('.').next()?;
    let mut ancestor = children.syntax_elements[class].parent;
    while let Some(index) = ancestor {
        if shadows
            .get(&index)
            .is_some_and(|names| names.contains(root))
        {
            return None;
        }
        ancestor = children.syntax_elements[index].parent;
    }
    let arguments = children.child(call, "arguments")?;
    let templates = if let Some(object) = children.child(arguments, "object") {
        children
            .indices(object)
            .filter_map(|index| template(text, children, index))
            .collect()
    } else {
        vec![AngularTemplate::Unresolved {
            range: children.syntax_elements[arguments].range,
        }]
    };
    Some(AngularComponent {
        range: children.syntax_elements[class].range,
        templates,
    })
}

fn binding_shadows(
    text: &str,
    children: &Children<'_>,
    imports: &BTreeSet<String>,
) -> BTreeMap<usize, BTreeSet<String>> {
    let roots: BTreeSet<_> = imports
        .iter()
        .filter_map(|name| name.split('.').next())
        .collect();
    let mut shadows: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for index in 0..children.syntax_elements.len() {
        let Some((scope, bindings)) = scoped_bindings(children, index) else {
            continue;
        };
        let names = binding_names(text, children, bindings, &roots);
        if !names.is_empty() {
            shadows.entry(scope).or_default().extend(names);
        }
    }
    shadows
}

fn scoped_bindings(children: &Children<'_>, index: usize) -> Option<(usize, Vec<usize>)> {
    let syntax_element = &children.syntax_elements[index];
    let first_binding = |parent| {
        children
            .indices(parent)
            .find(|child| binding_kind(children.syntax_elements[*child].kind))
    };
    match syntax_element.kind {
        "formal_parameters" => Some((
            syntax_element.parent?,
            children.indices(index).filter_map(first_binding).collect(),
        )),
        "arrow_function" if children.child(index, "formal_parameters").is_none() => {
            Some((index, vec![children.child(index, "identifier")?]))
        }
        "variable_declarator" | "function_declaration" => {
            let binding = if syntax_element.kind == "function_declaration" {
                children.child(index, "identifier")?
            } else {
                first_binding(index)?
            };
            let mut ancestor = syntax_element.parent;
            for _ in 0..children.syntax_elements.len() {
                let scope = ancestor?;
                if matches!(
                    children.syntax_elements[scope].kind,
                    "statement_block" | "program"
                ) {
                    return Some((scope, vec![binding]));
                }
                ancestor = children.syntax_elements[scope].parent;
            }
            None
        }
        _ => None,
    }
}

fn binding_names(
    text: &str,
    children: &Children<'_>,
    mut pending: Vec<usize>,
    roots: &BTreeSet<&str>,
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    while let Some(binding) = pending.pop() {
        match children.syntax_elements[binding].kind {
            "identifier" | "shorthand_property_identifier_pattern" => {
                if let Some(name) = children
                    .text(binding, text)
                    .filter(|name| roots.contains(name))
                {
                    names.insert(name.to_owned());
                }
            }
            "assignment_pattern" | "object_assignment_pattern" => {
                if let Some(left) = children.indices(binding).next() {
                    pending.push(left);
                }
            }
            _ => pending.extend(
                children
                    .indices(binding)
                    .filter(|index| binding_kind(children.syntax_elements[*index].kind)),
            ),
        }
    }
    names
}

fn binding_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "shorthand_property_identifier_pattern"
            | "object_pattern"
            | "array_pattern"
            | "pair_pattern"
            | "rest_pattern"
            | "assignment_pattern"
            | "object_assignment_pattern"
    )
}

fn template(text: &str, children: &Children<'_>, index: usize) -> Option<AngularTemplate> {
    if children.syntax_elements[index].kind != "pair" {
        return None;
    }
    let mut members = children.indices(index);
    let key = members.next()?;
    let name = if children.syntax_elements[key].kind == "string" {
        children.literal(key, text)?.0
    } else {
        children.text(key, text)?
    };
    if !matches!(name, TEMPLATE | TEMPLATE_URL) {
        return None;
    }
    let value = members.next()?;
    Some(match children.literal(value, text) {
        Some((path, _)) if name == TEMPLATE_URL => AngularTemplate::External {
            path: path.to_owned(),
        },
        Some((_, range)) => AngularTemplate::Inline { range },
        None => AngularTemplate::Unresolved {
            range: children.syntax_elements[value].range,
        },
    })
}
