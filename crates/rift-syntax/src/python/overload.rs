use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::extract::byte_range;
use crate::failure::RiftError;
use crate::{
    ByteRange, PythonOverload, StaticBindingKind, StaticFacts, SyntaxLimits, SyntaxSymbol,
};

use super::{PARAMETER_KIND_WORD, python_kinds};

#[derive(Debug, Clone)]
struct TypingImport {
    statement: ByteRange,
    module: ByteRange,
    imported: Option<ByteRange>,
    binding: ByteRange,
    name: TypingName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypingName {
    Module,
    Overload,
    TypeChecking,
}

/// Bounded import bindings from the existing Python parse.
#[derive(Debug, Default)]
pub(super) struct OverloadContext {
    imports: BTreeMap<String, TypingImport>,
    shadowed: BTreeSet<String>,
    unknown: bool,
    stub_signatures: BTreeSet<ByteRange>,
    guarded_members: BTreeMap<ByteRange, (ByteRange, PythonOverload)>,
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
        let mut guarded = Vec::new();
        let mut writes_guard = false;
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
                        writes_guard |= left
                            .child_by_field_id(python_kinds().attribute_name.get())
                            .is_some_and(|name| {
                                source.get(name.byte_range()) == Some("TYPE_CHECKING")
                            });
                    }
                }
                "function_definition" | "class_definition" => {
                    if syntax_element.kind_id() == python_kinds().function {
                        if stub && written_stub_signature(syntax_element) {
                            context.stub_signatures.insert(byte_range(syntax_element)?);
                        }
                        if let Some((suite, condition)) = guarded_suite(syntax_element) {
                            guarded.push((syntax_element, suite, condition));
                        }
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
        if !context.unknown && !writes_guard {
            for (function, suite, condition) in guarded {
                if context.type_checking(condition, source) {
                    let state = context.decorated_state(function, wrapper(function), source)?;
                    context
                        .guarded_members
                        .insert(byte_range(function)?, (byte_range(suite)?, state));
                }
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
            let name = match (valid_module, from, source.get(imported.byte_range())) {
                (true, false, _) => Some(TypingName::Module),
                (true, true, Some("overload")) => Some(TypingName::Overload),
                (true, true, Some("TYPE_CHECKING")) => Some(TypingName::TypeChecking),
                _ => None,
            };
            let Some(name) = name.filter(|_| !self.imports.contains_key(bound_name)) else {
                self.shadowed.insert(bound_name.to_owned());
                continue;
            };
            self.imports.insert(
                bound_name.to_owned(),
                TypingImport {
                    statement: byte_range(statement)?,
                    module: byte_range(module)?,
                    imported: from.then(|| byte_range(imported)).transpose()?,
                    binding: byte_range(binding)?,
                    name,
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
        let parent = wrapper(syntax_element);
        let allowed_scope = parent.parent().is_some_and(direct_suite);
        if !allowed_scope || self.unknown {
            if self
                .guarded_members
                .get(&byte_range(syntax_element)?)
                .is_some_and(|(_, state)| *state == PythonOverload::Ordinary)
            {
                return Ok(PythonOverload::GuardedImplementation);
            }
            return Ok(PythonOverload::Unknown);
        }
        self.decorated_state(syntax_element, parent, source)
    }

    fn decorated_state(
        &self,
        syntax_element: Node<'_>,
        parent: Node<'_>,
        source: &str,
    ) -> Result<PythonOverload, RiftError> {
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
            let (binding, kind) = match expression.kind() {
                "identifier" => (text, TypingName::Overload),
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
                    (binding, TypingName::Module)
                }
                _ => return Ok(PythonOverload::Unknown),
            };
            let Some(import) = self.imports.get(binding).filter(|import| {
                !self.shadowed.contains(binding)
                    && kind == import.name
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

    fn type_checking(&self, condition: Node<'_>, source: &str) -> bool {
        let kinds = python_kinds();
        let (binding, kind) = if condition.kind_id() == kinds.attribute {
            let Some((object, member)) = condition
                .child_by_field_id(kinds.object.get())
                .zip(condition.child_by_field_id(kinds.attribute_name.get()))
            else {
                return false;
            };
            if source.get(member.byte_range()) != Some("TYPE_CHECKING") {
                return false;
            }
            (object, TypingName::Module)
        } else {
            (condition, TypingName::TypeChecking)
        };
        binding.kind_id() == kinds.identifier
            && source.get(binding.byte_range()).is_some_and(|name| {
                !self.shadowed.contains(name)
                    && self.imports.get(name).is_some_and(|import| {
                        import.name == kind && import.statement.end <= condition.start_byte() as u64
                    })
            })
    }

    fn family_state<'family>(
        &'family self,
        symbol: &'family SyntaxSymbol,
    ) -> Option<(Option<ByteRange>, &'family PythonOverload)> {
        if let Some((suite, state)) = self.guarded_members.get(&symbol.item_range) {
            return Some((Some(*suite), state));
        }
        symbol.python_overload.as_ref().map(|state| (None, state))
    }
}

fn wrapper(function: Node<'_>) -> Node<'_> {
    function
        .parent()
        .filter(|parent| parent.kind_id() == python_kinds().decorated)
        .unwrap_or(function)
}

fn direct_suite(suite: Node<'_>) -> bool {
    let kinds = python_kinds();
    suite.kind_id() == kinds.module
        || (suite.kind_id() == kinds.block
            && suite
                .parent()
                .is_some_and(|parent| parent.kind_id() == kinds.class))
}

fn guarded_suite(function: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let kinds = python_kinds();
    let suite = wrapper(function).parent()?;
    let guarded = suite.parent()?;
    (suite.kind_id() == kinds.block
        && guarded.kind_id() == kinds.if_statement
        && !guarded.has_error()
        && guarded.child_by_field_id(kinds.consequence.get()) == Some(suite)
        && guarded.parent().is_some_and(direct_suite))
    .then(|| Some((suite, guarded.child_by_field_id(kinds.condition.get())?)))
    .flatten()
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
    let owners = overload_members(symbols, facts, counts, overloads);
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
    overloads: &OverloadContext,
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
        let Some((suite, state)) = overloads.family_state(last) else {
            continue;
        };
        let signatures = match state {
            PythonOverload::Ordinary => earlier,
            PythonOverload::Overload { .. }
                if family
                    .iter()
                    .all(|signature| overloads.stub_signatures.contains(&signature.item_range)) =>
            {
                family.as_slice()
            }
            _ => continue,
        };
        if earlier
            .iter()
            .any(|signature| signature.item_range.end > last.item_range.start)
            || !signatures.iter().all(|signature| {
                overloads
                    .family_state(signature)
                    .is_some_and(|(written, state)| {
                        written == suite && matches!(state, PythonOverload::Overload { .. })
                    })
                    && signature
                        .name_range
                        .and_then(|name| bindings.get(&name))
                        .is_some_and(|signature_binding| {
                            signature_binding.scope == binding.scope
                                && matches!(
                                    signature_binding.kind,
                                    StaticBindingKind::Local
                                        | StaticBindingKind::PythonFunction { .. }
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
