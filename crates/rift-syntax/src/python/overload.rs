use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::extract::byte_range;
use crate::failure::RiftError;
use crate::{
    ByteRange, PythonOverload, StaticBindingKind, StaticFacts, SyntaxLimits, SyntaxSymbol,
};

use super::PARAMETER_KIND_WORD;

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
    stub_signatures: BTreeSet<ByteRange>,
}

impl OverloadContext {
    pub(super) fn new(
        root: Node<'_>,
        source: &str,
        limits: SyntaxLimits,
        stub: bool,
    ) -> Result<Self, RiftError> {
        let mut context = Self::default();
        let stub = stub && !root.has_error();
        let mut cursor = root.walk();
        for statement in root
            .named_children(&mut cursor)
            .take(limits.syntax_elements_max())
        {
            context.import(statement, source)?;
        }
        let mut work = vec![(root, 0_usize)];
        let mut visited = 0_usize;
        while let Some((syntax_element, depth)) = work.pop() {
            if visited >= limits.syntax_elements_max() || depth > limits.syntax_depth_max() {
                context.unknown = true;
                break;
            }
            visited += 1;
            match syntax_element.kind() {
                "assignment" | "augmented_assignment" => {
                    if let Some(left) = syntax_element.child_by_field_name("left") {
                        context.shadow(left, source);
                    }
                }
                "function_definition" | "class_definition" => {
                    if stub
                        && syntax_element.kind() == "function_definition"
                        && written_stub_signature(syntax_element)
                    {
                        context.stub_signatures.insert(byte_range(syntax_element)?);
                    }
                    if let Some(name) = syntax_element.child_by_field_name("name") {
                        context.shadow(name, source);
                    }
                }
                "parameters" | "lambda_parameters" => {
                    let mut cursor = syntax_element.walk();
                    for child in syntax_element.named_children(&mut cursor) {
                        context.shadow(child, source);
                    }
                }
                "import_statement" | "import_from_statement"
                    if syntax_element
                        .parent()
                        .is_none_or(|parent| parent.id() != root.id()) =>
                {
                    context.shadow_import(syntax_element, source);
                }
                _ => {}
            }
            let mut cursor = syntax_element.walk();
            for child in syntax_element.named_children(&mut cursor) {
                if work.len().saturating_add(visited) >= limits.syntax_elements_max() {
                    context.unknown = true;
                    break;
                }
                work.push((child, depth.saturating_add(1)));
            }
        }
        Ok(context)
    }

    fn shadow(&mut self, syntax_element: Node<'_>, source: &str) {
        if let Some(name) = source.get(syntax_element.byte_range())
            && (self.imports.contains_key(name) || matches!(name, "classmethod" | "staticmethod"))
        {
            self.shadowed.insert(name.to_owned());
        }
    }

    fn shadow_import(&mut self, statement: Node<'_>, source: &str) {
        let module = statement.child_by_field_name("module_name");
        let mut cursor = statement.walk();
        for imported in statement.named_children(&mut cursor) {
            if Some(imported) == module {
                continue;
            }
            let bound = imported
                .child_by_field_name("alias")
                .or_else(|| imported.child_by_field_name("name"))
                .unwrap_or(imported);
            self.shadow(bound.named_child(0).unwrap_or(bound), source);
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

    pub(super) fn state(
        &self,
        syntax_element: Node<'_>,
        source: &str,
    ) -> Result<PythonOverload, RiftError> {
        let parent = syntax_element
            .parent()
            .filter(|parent| parent.kind() == "decorated_definition")
            .unwrap_or(syntax_element);
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
        if parent.id() == syntax_element.id() {
            return Ok(PythonOverload::Ordinary);
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
                if self.shadowed.contains(text) {
                    return Ok(PythonOverload::Unknown);
                }
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

/// Keeps repeated parameter names within distinct physical declarations of
/// one ordered overload family in the same immediate lexical scope.
pub(super) fn repeated_parameters(
    symbols: &[SyntaxSymbol],
    facts: &StaticFacts,
    counts: &BTreeMap<&str, usize>,
    eligible: &BTreeMap<ByteRange, ByteRange>,
    overloads: &OverloadContext,
) -> BTreeSet<ByteRange> {
    let owners = overload_members(symbols, facts, counts, &overloads.stub_signatures);
    if owners.is_empty() {
        return BTreeSet::new();
    }
    let mut candidates: BTreeMap<&str, BTreeMap<ByteRange, ByteRange>> = BTreeMap::new();
    for parameter in symbols
        .iter()
        .filter(|symbol| symbol.kind == PARAMETER_KIND_WORD)
    {
        if let Some((name, owner)) = parameter
            .name_range
            .and_then(|name| Some((name, *eligible.get(&name)?)))
            && owners
                .get(&owner)
                .is_some_and(|owner| Some(*owner) == parameter.container.as_deref())
        {
            candidates
                .entry(&parameter.qualified_name)
                .or_default()
                .insert(owner, name);
        }
    }
    candidates
        .into_iter()
        .filter(|(name, family)| family.len() > 1 && counts.get(*name) == Some(&family.len()))
        .flat_map(|(_, family)| family.into_values())
        .collect()
}

fn overload_members<'symbols>(
    symbols: &'symbols [SyntaxSymbol],
    facts: &StaticFacts,
    counts: &BTreeMap<&str, usize>,
    stub_signatures: &BTreeSet<ByteRange>,
) -> BTreeMap<ByteRange, &'symbols str> {
    let mut families: BTreeMap<&str, Vec<&SyntaxSymbol>> = BTreeMap::new();
    for callable in symbols.iter().filter(|symbol| {
        symbol.python_overload.is_some()
            && counts
                .get(symbol.qualified_name.as_str())
                .is_some_and(|count| *count > 1)
    }) {
        families
            .entry(&callable.qualified_name)
            .or_default()
            .push(callable);
    }
    if families.is_empty() {
        return BTreeMap::new();
    }
    let bindings = facts
        .bindings()
        .iter()
        .map(|binding| (binding.name, binding))
        .collect::<BTreeMap<_, _>>();
    let mut owners = BTreeMap::new();
    for (name, mut family) in families {
        if counts.get(name) != Some(&family.len()) {
            continue;
        }
        family.sort_by_key(|callable| callable.item_range);
        let Some((last, earlier)) = family.split_last() else {
            continue;
        };
        let Some(binding) = last.name_range.and_then(|name| bindings.get(&name)) else {
            continue;
        };
        let signatures = match last.python_overload {
            Some(PythonOverload::Ordinary) => earlier,
            Some(PythonOverload::Overload { .. })
                if family
                    .iter()
                    .all(|signature| stub_signatures.contains(&signature.item_range)) =>
            {
                family.as_slice()
            }
            _ => continue,
        };
        if earlier
            .iter()
            .any(|signature| signature.item_range.end > last.item_range.start)
            || !signatures.iter().all(|signature| {
                matches!(
                    signature.python_overload,
                    Some(PythonOverload::Overload { .. })
                ) && signature
                    .name_range
                    .and_then(|name| bindings.get(&name))
                    .is_some_and(|signature_binding| {
                        signature_binding.scope == binding.scope
                            && matches!(
                                signature_binding.kind,
                                StaticBindingKind::Local | StaticBindingKind::PythonFunction { .. }
                            )
                    })
            })
        {
            continue;
        }
        owners.extend(
            family
                .into_iter()
                .map(|callable| (callable.item_range, name)),
        );
    }
    owners
}

/// A stub family can consist wholly of written placeholders in one selected file.
fn written_stub_signature(function: Node<'_>) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    let mut statements = body
        .named_children(&mut cursor)
        .filter(|statement| statement.kind() != "comment");
    let (Some(statement), None) = (statements.next(), statements.next()) else {
        return false;
    };
    statement.kind() == "pass_statement"
        || (statement.kind() == "expression_statement"
            && statement.named_child_count() == 1
            && statement
                .named_child(0)
                .is_some_and(|value| value.kind() == "ellipsis"))
}
