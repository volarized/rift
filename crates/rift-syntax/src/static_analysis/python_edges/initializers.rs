//! Authored fields and keyword arguments for generated Python initializers.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::EdgePythonKinds;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{
    StaticCall, StaticFactsParts, StaticPythonInitializer, StaticPythonInitializerField, kind,
};

struct InitializerKinds {
    statement: u16,
    pass: u16,
    string: u16,
    literals: [u16; 7],
    splats: [u16; 2],
}

fn kinds() -> &'static InitializerKinds {
    static KINDS: OnceLock<InitializerKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = tree_sitter_python::LANGUAGE.into();
        let id = |name| kind(&grammar, name);
        InitializerKinds {
            statement: id("expression_statement"),
            pass: id("pass_statement"),
            string: id("string"),
            literals: [
                "none", "true", "false", "string", "integer", "float", "ellipsis",
            ]
            .map(id),
            splats: ["dictionary_splat", "list_splat"].map(id),
        }
    })
}

impl EdgePythonKinds {
    pub(super) fn edge_initializer_keywords(
        &self,
        expression: Node<'_>,
        call: StaticCall,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(arguments) = expression.child_by_field_name("arguments") else {
            return Ok(());
        };
        let mut cursor = arguments.walk();
        let mut keywords = Vec::new();
        let mut seen = BTreeSet::new();
        for argument in arguments
            .named_children(&mut cursor)
            .filter(|item| !item.is_extra())
        {
            if kinds().splats.contains(&argument.kind_id()) || argument.has_error() {
                return Ok(());
            }
            if argument.kind_id() == self.keyword_argument
                && let Some(name) = argument.child_by_field_id(self.name.get())
            {
                let Some(spelling) = source.get(name.byte_range()) else {
                    return Ok(());
                };
                if !seen.insert(spelling) {
                    return Ok(());
                }
                keywords.push((call.range, byte_range(name)?));
            }
        }
        facts.python_keyword_arguments.extend(keywords);
        Ok(())
    }

    pub(super) fn edge_initializer(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some((decorator, keyword_only)) = self.initializer_decorator(visited, source) else {
            return Ok(());
        };
        let class = visited.syntax_element();
        if class.has_error() || class.child_by_field_name("type_parameters").is_some() {
            return Ok(());
        }
        let base = if let Some(bases) = class.child_by_field_id(self.superclasses) {
            let mut cursor = bases.walk();
            let mut bases = bases
                .named_children(&mut cursor)
                .filter(|item| !item.is_extra());
            let base = bases.next();
            if bases.next().is_some()
                || base.is_some_and(|base| {
                    ![self.identifier, self.attribute].contains(&base.kind_id())
                })
            {
                return Ok(());
            }
            base.map(byte_range).transpose()?
        } else {
            None
        };
        let Some(fields) = self.initializer_fields(class, source, keyword_only, base.is_some())?
        else {
            return Ok(());
        };
        facts.python_initializers.push(StaticPythonInitializer {
            class: byte_range(class)?,
            decorator: byte_range(decorator)?,
            base,
            fields,
        });
        Ok(())
    }

    fn initializer_decorator<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
        source: &str,
    ) -> Option<(Node<'tree>, bool)> {
        let wrapper = visited.parent()?.syntax_element();
        if wrapper.kind_id() != self.decorated {
            return None;
        }
        let mut cursor = wrapper.walk();
        let mut decorators = wrapper
            .named_children(&mut cursor)
            .filter(|item| item.kind_id() == self.decorator);
        let decorator = decorators.next()?;
        let expression = decorator
            .named_children(&mut decorator.walk())
            .find(|item| !item.is_extra())?;
        if decorators.next().is_some() {
            return None;
        }
        if [self.identifier, self.attribute].contains(&expression.kind_id()) {
            return Some((expression, false));
        }
        if expression.kind_id() != self.call {
            return None;
        }
        let callee = expression.child_by_field_id(self.function)?;
        if ![self.identifier, self.attribute].contains(&callee.kind_id()) {
            return None;
        }
        let arguments = expression.child_by_field_name("arguments")?;
        let mut cursor = arguments.walk();
        let mut keyword_only = None;
        let mut init = None;
        for option in arguments
            .named_children(&mut cursor)
            .filter(|item| !item.is_extra())
        {
            if option.kind_id() != self.keyword_argument {
                return None;
            }
            let name = source.get(option.child_by_field_id(self.name.get())?.byte_range())?;
            let value = source.get(option.child_by_field_id(self.default_value)?.byte_range())?;
            match (name, value) {
                ("kw_only", "True" | "False") if keyword_only.is_none() => {
                    keyword_only = Some(value == "True");
                }
                ("init", "True") if init.is_none() => init = Some(true),
                _ => return None,
            }
        }
        Some((callee, keyword_only.unwrap_or(false)))
    }

    fn initializer_fields(
        &self,
        class: Node<'_>,
        source: &str,
        keyword_only: bool,
        has_base: bool,
    ) -> Result<Option<Vec<StaticPythonInitializerField>>, RiftError> {
        let Some(body) = class.child_by_field_id(self.body) else {
            return Ok(None);
        };
        let mut fields = Vec::new();
        let mut names = BTreeSet::new();
        let mut after_default = has_base;
        for statement in body
            .named_children(&mut body.walk())
            .filter(|item| !item.is_extra())
        {
            if statement.kind_id() == kinds().pass {
                continue;
            }
            let definition = if statement.kind_id() == self.decorated {
                statement
                    .named_children(&mut statement.walk())
                    .find(|item| item.kind_id() == self.definitions[0])
            } else {
                (statement.kind_id() == self.definitions[0]).then_some(statement)
            };
            if let Some(definition) = definition {
                let Some(name) = definition
                    .child_by_field_id(self.name.get())
                    .and_then(|name| source.get(name.byte_range()))
                else {
                    return Ok(None);
                };
                if matches!(
                    name,
                    "__init__" | "__new__" | "__attrs_init_subclass__" | "__init_subclass__"
                ) || !names.insert(name)
                {
                    return Ok(None);
                }
                continue;
            }
            let Some(expression) = statement
                .named_child(0)
                .filter(|_| statement.kind_id() == kinds().statement)
            else {
                return Ok(None);
            };
            if expression.kind_id() == kinds().string {
                continue;
            }
            let Some(name) = expression.child_by_field_id(self.left).filter(|name| {
                expression.kind_id() == self.assignment[0] && name.kind_id() == self.identifier
            }) else {
                return Ok(None);
            };
            let Some(spelling) = source.get(name.byte_range()) else {
                return Ok(None);
            };
            if spelling.starts_with('_') || !names.insert(spelling) {
                return Ok(None);
            }
            let default = expression.child_by_field_id(self.right);
            if default.is_some_and(|value| !kinds().literals.contains(&value.kind_id())) {
                return Ok(None);
            }
            if let Some(annotation) = expression.child_by_field_id(self.type_field) {
                if !keyword_only && after_default && default.is_none() {
                    return Ok(None);
                }
                after_default |= default.is_some();
                fields.push(StaticPythonInitializerField {
                    name: byte_range(name)?,
                    annotation: byte_range(annotation)?,
                });
            }
        }
        Ok(Some(fields))
    }
}
