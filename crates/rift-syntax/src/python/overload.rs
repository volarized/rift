use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::extract::byte_range;
use crate::failure::RiftError;
use crate::{ByteRange, PythonOverload, SyntaxLimits};

#[derive(Debug, Clone)]
struct ImportedOverload {
    statement: ByteRange,
    module: ByteRange,
    imported: Option<ByteRange>,
    binding: ByteRange,
}

/// Bounded import bindings from the existing Python parse.
#[derive(Debug, Default)]
pub(super) struct OverloadContext {
    imports: BTreeMap<String, ImportedOverload>,
    shadowed: BTreeSet<String>,
    unknown: bool,
}

impl OverloadContext {
    pub(super) fn new(
        root: Node<'_>,
        source: &str,
        limits: SyntaxLimits,
    ) -> Result<Self, RiftError> {
        let mut context = Self::default();
        let mut cursor = root.walk();
        for statement in root
            .named_children(&mut cursor)
            .take(limits.syntax_nodes_max())
        {
            context.import(statement, source)?;
        }
        let mut work = vec![(root, 0_usize)];
        let mut visited = 0_usize;
        while let Some((node, depth)) = work.pop() {
            if visited >= limits.syntax_nodes_max() || depth > limits.syntax_depth_max() {
                context.unknown = true;
                break;
            }
            visited += 1;
            match node.kind() {
                "assignment" | "augmented_assignment" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        context.shadow(left, source);
                    }
                }
                "function_definition" | "class_definition" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        context.shadow(name, source);
                    }
                }
                "parameters" | "lambda_parameters" => {
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        context.shadow(child, source);
                    }
                }
                "import_statement" | "import_from_statement"
                    if node.parent().is_none_or(|parent| parent.id() != root.id()) =>
                {
                    context.unknown = true;
                }
                _ => {}
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if work.len().saturating_add(visited) >= limits.syntax_nodes_max() {
                    context.unknown = true;
                    break;
                }
                work.push((child, depth.saturating_add(1)));
            }
        }
        Ok(context)
    }

    fn shadow(&mut self, node: Node<'_>, source: &str) {
        if let Some(name) = source.get(node.byte_range())
            && self.imports.contains_key(name)
        {
            self.shadowed.insert(name.to_owned());
        }
    }

    fn import(&mut self, statement: Node<'_>, source: &str) -> Result<(), RiftError> {
        let from = statement.kind() == "import_from_statement";
        if !from && statement.kind() != "import_statement" {
            return Ok(());
        }
        let module = if from {
            statement.child_by_field_name("module_name")
        } else {
            None
        };
        let mut cursor = statement.walk();
        for name in statement.named_children(&mut cursor) {
            if module.is_some_and(|module| module.id() == name.id()) {
                continue;
            }
            let (imported, binding) = if name.kind() == "aliased_import" {
                let Some(imported) = name.child_by_field_name("name") else {
                    continue;
                };
                let Some(binding) = name.child_by_field_name("alias") else {
                    continue;
                };
                (imported, binding)
            } else if name.kind() == "dotted_name" {
                (name, name)
            } else {
                continue;
            };
            let Some(bound_name) = source.get(binding.byte_range()) else {
                continue;
            };
            let module = module.unwrap_or(imported);
            let valid_module = matches!(
                source.get(module.byte_range()),
                Some("typing" | "typing_extensions")
            );
            let valid =
                valid_module && (!from || source.get(imported.byte_range()) == Some("overload"));
            if !valid || self.imports.contains_key(bound_name) {
                self.shadowed.insert(bound_name.to_owned());
                continue;
            }
            self.imports.insert(
                bound_name.to_owned(),
                ImportedOverload {
                    statement: byte_range(statement)?,
                    module: byte_range(module)?,
                    imported: from.then(|| byte_range(imported)).transpose()?,
                    binding: byte_range(binding)?,
                },
            );
        }
        Ok(())
    }

    pub(super) fn state(&self, node: Node<'_>, source: &str) -> Result<PythonOverload, RiftError> {
        let Some(parent) = node
            .parent()
            .filter(|parent| parent.kind() == "decorated_definition")
        else {
            return Ok(PythonOverload::Ordinary);
        };
        let allowed_scope = parent.parent().is_some_and(|scope| {
            scope.kind() == "module"
                || (scope.kind() == "block"
                    && scope
                        .parent()
                        .is_some_and(|outer| outer.kind() == "class_definition"))
        });
        if !allowed_scope || self.unknown {
            return Ok(PythonOverload::Unknown);
        }
        let mut found = None;
        let mut cursor = parent.walk();
        for decorator in parent
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "decorator")
        {
            let Some(expression) = decorator.named_child(0) else {
                return Ok(PythonOverload::Unknown);
            };
            let Some(text) = source.get(expression.byte_range()) else {
                return Ok(PythonOverload::Unknown);
            };
            if matches!(text, "classmethod" | "staticmethod") {
                continue;
            }
            let (binding, qualified) = match expression.kind() {
                "identifier" => (text, false),
                "attribute" => {
                    let Some(object) = expression.child_by_field_name("object") else {
                        return Ok(PythonOverload::Unknown);
                    };
                    let Some(attribute) = expression.child_by_field_name("attribute") else {
                        return Ok(PythonOverload::Unknown);
                    };
                    if source.get(attribute.byte_range()) != Some("overload") {
                        return Ok(PythonOverload::Unknown);
                    }
                    let Some(binding) = source.get(object.byte_range()) else {
                        return Ok(PythonOverload::Unknown);
                    };
                    (binding, true)
                }
                _ => return Ok(PythonOverload::Unknown),
            };
            let Some(import) = self.imports.get(binding).filter(|import| {
                !self.shadowed.contains(binding)
                    && qualified == import.imported.is_none()
                    && import.statement.end <= decorator.start_byte() as u64
            }) else {
                return Ok(PythonOverload::Unknown);
            };
            if found.is_some() {
                return Ok(PythonOverload::Unknown);
            }
            found = Some(PythonOverload::Overload {
                import_statement: import.statement,
                module: import.module,
                imported: import.imported,
                binding: import.binding,
                decorator: byte_range(expression)?,
            });
        }
        Ok(found.unwrap_or(PythonOverload::Ordinary))
    }
}
