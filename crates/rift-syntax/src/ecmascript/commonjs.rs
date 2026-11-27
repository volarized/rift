use super::{
    DEFAULT_EXPORT_NAME, EcmaScriptGrammarField, EcmaScriptKinds, EcmaScriptSymbolKind,
    ExportTarget, named_children,
};
use crate::{
    ByteRange, StaticCommonJsExport, SyntaxExportBinding, SyntaxExportKind, SyntaxLimits,
    SyntaxSource, extract,
};
use rift_error::{RiftError, errors};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

/// Checks the exact direct statements of a superseded `CommonJS` placeholder.
pub(crate) fn commonjs_class_export_initializations(
    source: &str,
    limits: SyntaxLimits,
    initializations: &BTreeMap<ByteRange, ByteRange>,
) -> BTreeSet<ByteRange> {
    if initializations.is_empty()
        || initializations.len() > limits.syntax_elements_max()
        || source.len() > limits.source_bytes_max()
    {
        return BTreeSet::new();
    }
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&crate::javascript::javascript_grammar())
        .is_err()
    {
        return BTreeSet::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return BTreeSet::new();
    };
    let root = tree.root_node();
    if root.has_error() || !checked_placeholder_wrapper(root, source, limits) {
        return BTreeSet::new();
    }
    initializations
        .iter()
        .filter_map(|(&later, &earlier)| {
            let earlier = exact_named_node(root, earlier, "member_expression")?;
            let first = earlier.parent()?;
            if first.child_by_field_name("left") != Some(earlier) {
                return None;
            }
            let last = exact_named_node(root, later, "assignment_expression")?;
            let last_key = last.child_by_field_name("left")?;
            let first_key = named_placeholder_member(earlier, source)?;
            (first_key == named_placeholder_member(last_key, source)?
                && first.end_byte() <= last.start_byte()
                && pure_placeholder_statement(first, root, source)
                && direct_expression_statement(last, root)
                && last.child_by_field_name("right")?.kind() == "identifier")
                .then_some(later)
        })
        .collect()
}

fn exact_named_node<'tree>(root: Node<'tree>, range: ByteRange, kind: &str) -> Option<Node<'tree>> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    let node = root.named_descendant_for_byte_range(start, end)?;
    (node.kind() == kind && node.start_byte() == start && node.end_byte() == end).then_some(node)
}

fn named_placeholder_member<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    let receiver = node.child_by_field_name("object")?;
    let property = node.child_by_field_name("property")?;
    (node.kind() == "member_expression"
        && receiver.kind() == "identifier"
        && text(source, receiver) == "exports"
        && property.kind() == "property_identifier"
        && !matches!(text(source, property), "__proto__" | "__esModule"))
    .then(|| text(source, property))
}

/// The full assignment chain writes only named exports and ends in literal undefined.
fn pure_placeholder_statement(mut node: Node<'_>, root: Node<'_>, source: &str) -> bool {
    while let Some(parent) = node
        .parent()
        .filter(|node| node.kind() == "assignment_expression")
    {
        if parent.child_by_field_name("right") != Some(node) {
            return false;
        }
        node = parent;
    }
    if !direct_expression_statement(node, root) {
        return false;
    }
    while node.kind() == "assignment_expression" {
        if node
            .child_by_field_name("left")
            .and_then(|left| named_placeholder_member(left, source))
            .is_none()
        {
            return false;
        }
        let Some(value) = node.child_by_field_name("right") else {
            return false;
        };
        node = value;
    }
    node.kind() == "unary_expression" && text(source, node) == "void 0"
}

fn direct_expression_statement(node: Node<'_>, root: Node<'_>) -> bool {
    node.parent().is_some_and(|statement| {
        statement.kind() == "expression_statement" && statement.parent() == Some(root)
    })
}

/// The exported object stays with known writes and an optional compiler marker.
fn checked_placeholder_wrapper(root: Node<'_>, source: &str, limits: SyntaxLimits) -> bool {
    let mut pending = vec![(root, 0_usize)];
    let mut cursor = root.walk();
    let mut visited = 0_usize;
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        if depth > limits.syntax_depth_max()
            || visited
                .saturating_add(pending.len())
                .saturating_add(node.named_child_count())
                > limits.syntax_elements_max()
        {
            return false;
        }
        if matches!(
            node.kind(),
            "identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ) {
            let name = text(source, node);
            if name.contains('\\')
                || matches!(name, "module" | "eval")
                || (name == "exports" && !placeholder_exports_use(node, root, source))
                || (name == "Object" && !placeholder_marker_receiver(node, root, source))
            {
                return false;
            }
        }
        extract::push_named_children(&mut pending, &mut cursor, node, |child| (child, depth + 1));
    }
    true
}

fn placeholder_exports_use(node: Node<'_>, root: Node<'_>, source: &str) -> bool {
    node.parent().is_some_and(|parent| {
        (parent.child_by_field_name("object") == Some(node)
            && named_placeholder_member(parent, source).is_some()
            && parent.parent().is_some_and(|assignment| {
                assignment.kind() == "assignment_expression"
                    && assignment.child_by_field_name("left") == Some(parent)
            }))
            || (parent.kind() == "arguments"
                && parent
                    .parent()
                    .is_some_and(|call| placeholder_marker(call, root, source)))
    })
}

fn placeholder_marker_receiver(node: Node<'_>, root: Node<'_>, source: &str) -> bool {
    node.parent().is_some_and(|member| {
        member.child_by_field_name("object") == Some(node)
            && member.parent().is_some_and(|call| {
                call.child_by_field_name("function") == Some(member)
                    && placeholder_marker(call, root, source)
            })
    })
}

/// The boolean module marker gives another named property a value without moving the object.
fn placeholder_marker(call: Node<'_>, root: Node<'_>, source: &str) -> bool {
    if call.kind() != "call_expression"
        || !direct_expression_statement(call, root)
        || call
            .child_by_field_name("function")
            .is_none_or(|function| text(source, function) != "Object.defineProperty")
    {
        return false;
    }
    let args = call_arguments(call);
    let [receiver, property, descriptor] = args.as_slice() else {
        return false;
    };
    text(source, *receiver) == "exports"
        && matches!(text(source, *property), "\"__esModule\"" | "'__esModule'")
        && descriptor.kind() == "object"
        && descriptor.named_child_count() == 1
        && descriptor.named_child(0).is_some_and(|pair| {
            pair.kind() == "pair"
                && pair
                    .child_by_field_name("key")
                    .is_some_and(|key| text(source, key) == "value")
                && pair
                    .child_by_field_name("value")
                    .is_some_and(|value| value.kind() == "true")
        })
}

/// What the module body assigns to its exports, beside the recorded bindings.
#[derive(Default)]
pub(super) struct CommonJsExports<'tree> {
    /// Every assignment of the source, in walk order.
    pub(super) assignments: Vec<Node<'tree>>,
    pub(super) closed_export: Option<StaticCommonJsExport>,
    /// A single root wrapper assignment with a written callable composite.
    pub(super) expression_export: bool,
    /// Assignments that declare their own export at module scope.
    pub(super) declared: BTreeSet<usize>,
    /// Identifiers an export names, whose declarations the module exports.
    pub(super) named: Vec<Node<'tree>>,
    /// Object literals a module value or an export holds: their members are exported.
    pub(super) objects: BTreeSet<usize>,
    /// Assignments that declare a value under a name a module-scope declaration takes.
    pub(super) shadowing: BTreeSet<usize>,
}

pub(super) fn record<'tree>(
    root: Node<'tree>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    kinds: &EcmaScriptKinds,
    allow_literal: bool,
    bindings: &mut BTreeMap<usize, SyntaxExportBinding>,
) -> Result<CommonJsExports<'tree>, RiftError> {
    let mut captured = capture_assignments(root, source, limits, kinds)?;
    let synchronized = captured.synchronized.and_then(|(outer, inner)| {
        if !captured.blocked.contains(&root.id())
            && closed_wrapper_uses(root, Some(outer), inner, source.text, kinds)
        {
            Some(inner)
        } else {
            captured.blocked.insert(root.id());
            None
        }
    });
    let body = ModuleBody {
        root,
        source: source.text,
        kinds,
        replaced: &captured.replaced,
        synchronized,
    };
    let mut closed_export = None;
    let mut direct_function = None;
    let mut shadowing = BTreeSet::new();
    for &syntax_element in &captured.assignments {
        if synchronized != Some(syntax_element)
            && !module_execution(syntax_element, &captured.blocked)
        {
            continue;
        }
        let direct = captured.direct_default == Some(syntax_element);
        if allow_literal
            && direct
            && let Some(literal) = body.closed_literal(syntax_element)
        {
            closed_export = Some((
                extract::byte_range(syntax_element)?,
                false,
                Some(extract::byte_range(literal)?),
            ));
        }
        let Some(binding) = assignment(syntax_element, source.text, kinds)? else {
            record_conditional_forward(syntax_element, source.text, kinds, bindings)?;
            continue;
        };
        if binding.kind == SyntaxExportKind::Named && !body.keeps(syntax_element) {
            continue;
        }
        if body.is_closed_default(syntax_element, &binding, direct) {
            closed_export = Some((binding.range, synchronized == Some(syntax_element), None));
        } else if allow_literal
            && direct
            && body.is_closed_written_function(syntax_element, &binding)
        {
            direct_function = Some(binding.range);
        }
        if declares_taken_name(
            syntax_element,
            &binding,
            source.text,
            kinds,
            &captured.names,
        ) {
            shadowing.insert(syntax_element.id());
        }
        bindings.insert(syntax_element.id(), binding);
    }
    if !captured.blocked.contains(&root.id()) {
        record_aliases(&body, &captured, bindings)?;
        record_calls(&body, &captured.calls, bindings)?;
    }
    let closed_export = closed_export
        .map(|(assignment, synchronized, literal)| {
            let initialized = if literal.is_some() {
                Vec::new()
            } else {
                initialized_exports(root, source.text, kinds, bindings)?
            };
            Ok::<_, RiftError>(StaticCommonJsExport {
                assignment,
                synchronized,
                initialized,
                literal,
            })
        })
        .transpose()?;
    let mut exports = exported(&body, &captured, bindings);
    exports.expression_export = allow_literal
        && closed_export.is_none()
        && !captured.blocked.contains(&root.id())
        && captured.direct_default.is_some_and(|assignment| {
            !captured.bound.contains_key("Object")
                && !captured.rebound.contains("Object")
                && kinds
                    .assigned_value(assignment)
                    .is_some_and(|value| written_callable_composite(value, source.text, kinds))
                && closed_wrapper_uses(root, None, assignment, source.text, kinds)
        });
    exports.closed_export = closed_export.or_else(|| {
        direct_function.map(|assignment| StaticCommonJsExport {
            assignment,
            synchronized: false,
            initialized: Vec::new(),
            literal: None,
        })
    });
    exports.shadowing = shadowing;
    exports.assignments = captured.assignments;
    Ok(exports)
}

fn written_callable_composite(value: Node<'_>, source: &str, kinds: &EcmaScriptKinds) -> bool {
    if value.kind() != "call_expression"
        || value.child_by_field_name("optional_chain").is_some()
        || value
            .child_by_field_name("function")
            .is_none_or(|callee| text(source, callee) != "Object.assign")
    {
        return false;
    }
    let Some(arguments) = value.child_by_field_name("arguments") else {
        return false;
    };
    let mut children = named_children(arguments).filter(|child| !child.is_extra());
    children
        .next()
        .is_some_and(|first| kinds.function_values.contains(&first.kind_id()))
        && children
            .next()
            .is_some_and(|next| next.kind_id() == kinds.object_literal)
        && children.all(|argument| argument.kind_id() == kinds.object_literal)
}

/// The module body and the writes that replace or synchronize its exported object.
struct ModuleBody<'capture, 'tree> {
    root: Node<'tree>,
    source: &'capture str,
    kinds: &'capture EcmaScriptKinds,
    /// Scope of each `module.exports = value`, with the end of its last write there.
    replaced: &'capture BTreeMap<usize, usize>,
    /// The checked `module.exports` write that also updates `exports`.
    synchronized: Option<Node<'tree>>,
}

impl<'tree> ModuleBody<'_, 'tree> {
    /// The end of the last `module.exports = value` of the module body.
    fn replaced_until(&self) -> Option<usize> {
        self.replaced.get(&self.root.id()).copied()
    }

    /// Whether `exports` names the object the last replacement assigned.
    fn synchronizes(&self, end: usize) -> bool {
        self.synchronized
            .is_some_and(|inner| inner.end_byte() == end)
    }

    /// Whether the named export `assignment` writes survives every replacement.
    fn keeps(&self, assignment: Node<'_>) -> bool {
        let synchronized = self
            .synchronized
            .map(|inner| (self.root.id(), inner.end_byte()));
        kept_after_replacement(assignment, self.source, self.replaced, synchronized)
    }

    /// The literal a direct `module.exports = { .. }` assigns, when the file uses its
    /// wrapper names nowhere else.
    fn closed_literal(&self, assignment: Node<'tree>) -> Option<Node<'tree>> {
        self.kinds
            .child(assignment, EcmaScriptGrammarField::Right)
            .filter(|value| value.kind_id() == self.kinds.object_literal)
            .filter(|_| closed_wrapper_uses(self.root, None, assignment, self.source, self.kinds))
    }

    /// The written function that a closed module assigns directly to its default export.
    fn is_closed_written_function(
        &self,
        assignment: Node<'_>,
        binding: &SyntaxExportBinding,
    ) -> bool {
        let Some(value) = self.kinds.child(assignment, EcmaScriptGrammarField::Right) else {
            return false;
        };
        binding.kind == SyntaxExportKind::Default
            && value.kind() == "function_expression"
            && value
                .child_by_field_name("name")
                .is_some_and(|name| extract::byte_range(name).ok() == binding.local)
            && closed_wrapper_uses(self.root, None, assignment, self.source, self.kinds)
    }

    /// Whether the default export names a declaration and the file uses its wrapper names
    /// nowhere else.
    fn is_closed_default(
        &self,
        assignment: Node<'_>,
        binding: &SyntaxExportBinding,
        direct: bool,
    ) -> bool {
        binding.kind == SyntaxExportKind::Default
            && binding.local.is_some()
            && self
                .kinds
                .child(assignment, EcmaScriptGrammarField::Right)
                .is_some_and(|value| value.kind_id() == self.kinds.identifier)
            && (self.synchronized == Some(assignment)
                || (direct
                    && closed_wrapper_uses(self.root, None, assignment, self.source, self.kinds)))
    }
}

/// Reads what the recorded bindings export at module scope. A file with ECMAScript module
/// syntax runs no `CommonJS` wrapper, so its assignments export nothing.
fn exported<'tree>(
    body: &ModuleBody<'_, 'tree>,
    captured: &CapturedAssignments<'tree>,
    bindings: &BTreeMap<usize, SyntaxExportBinding>,
) -> CommonJsExports<'tree> {
    let ModuleBody {
        root,
        source,
        kinds,
        ..
    } = *body;
    let mut exports = CommonJsExports::default();
    if named_children(root).any(|statement| {
        statement.kind_id() == kinds.export_statement || statement.kind() == "import_statement"
    }) {
        return exports;
    }
    for &assignment in &captured.assignments {
        if !bindings.contains_key(&assignment.id()) || !module_statement(assignment, root, kinds) {
            continue;
        }
        let value = kinds.assigned_value(assignment);
        if value.is_some_and(|value| value.kind_id() == kinds.identifier) {
            exports.named.extend(value);
            continue;
        }
        exports.declared.insert(assignment.id());
        if let Some(object) = value.filter(|value| value.kind_id() == kinds.object_literal) {
            exports.objects.insert(object.id());
        }
    }
    for alias in captured
        .aliases
        .iter()
        .filter(|alias| bindings.contains_key(&alias.declarator.id()))
    {
        exports.named.push(alias.name);
        if alias.value.kind_id() == kinds.object_literal {
            exports.objects.insert(alias.value.id());
        }
    }
    for &call in captured
        .calls
        .iter()
        .filter(|call| bindings.contains_key(&call.id()))
    {
        exports.named.extend(
            defined_value(call, source, kinds).filter(|value| value.kind_id() == kinds.identifier),
        );
    }
    exports
}

/// Whether an assignment is a statement of the module body or the inner part of one.
fn module_statement(mut assignment: Node<'_>, root: Node<'_>, kinds: &EcmaScriptKinds) -> bool {
    while let Some(parent) = assignment.parent() {
        if parent.kind_id() == kinds.assignment_expression {
            assignment = parent;
            continue;
        }
        if parent.kind_id() == kinds.variable_declarator {
            return parent
                .parent()
                .is_some_and(|statement| statement.parent() == Some(root));
        }
        return parent.kind_id() == kinds.expression_statement && parent.parent() == Some(root);
    }
    false
}

/// A module-scope name bound to the object the module exports.
#[derive(Clone, Copy)]
struct Alias<'tree> {
    declarator: Node<'tree>,
    name: Node<'tree>,
    /// The value the declarator's assignment chain ends in.
    value: Node<'tree>,
    /// The `module.exports = value` of the chain. Without one the name is bound to the
    /// `exports` or `module.exports` object that already exists.
    write: Option<Node<'tree>>,
}

/// Reads `var app = exports = module.exports = value`, `var app = module.exports = value`,
/// `var app = exports` and `var app = module.exports` in the module body.
fn alias<'tree>(
    declarator: Node<'tree>,
    root: Node<'tree>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Option<Alias<'tree>> {
    if declarator.kind_id() != kinds.variable_declarator
        || declarator.has_error()
        || declarator.parent()?.parent()? != root
    {
        return None;
    }
    let name = kinds
        .child(declarator, EcmaScriptGrammarField::Name)
        .filter(|name| name.kind_id() == kinds.identifier)?;
    let mut value = kinds.child(declarator, EcmaScriptGrammarField::Value)?;
    let mut write = None;
    let mut chained = false;
    // Each step descends one admitted syntax element.
    while value.kind_id() == kinds.assignment_expression {
        let left = kinds.child(value, EcmaScriptGrammarField::Left)?;
        match source.get(left.byte_range())? {
            "module.exports" => write = Some(value),
            "exports" => {}
            _ => return None,
        }
        chained = true;
        value = kinds.child(value, EcmaScriptGrammarField::Right)?;
    }
    let bound = matches!(
        source.get(value.byte_range())?,
        "exports" | "module.exports"
    );
    (if chained {
        write.is_some() && !bound
    } else {
        bound
    })
    .then_some(Alias {
        declarator,
        name,
        value,
        write,
    })
}

/// Records the module value an alias names and the exports written through the alias.
fn record_aliases(
    body: &ModuleBody<'_, '_>,
    captured: &CapturedAssignments<'_>,
    bindings: &mut BTreeMap<usize, SyntaxExportBinding>,
) -> Result<(), RiftError> {
    let ModuleBody {
        root,
        source,
        kinds,
        ..
    } = *body;
    let aliases = &captured.aliases;
    let mut current = BTreeMap::new();
    for alias in aliases {
        let name = text(source, alias.name);
        // A name bound again anywhere can hold another value when the module body runs on.
        if !alias_is_current(body, alias)
            || captured.rebound.contains(name)
            || captured.bound.get(name) != Some(&1)
        {
            continue;
        }
        current.insert(name, alias);
        if let Some(write) = alias.write {
            let binding = match assignment(write, source, kinds)? {
                Some(binding) => binding,
                None => SyntaxExportBinding {
                    kind: SyntaxExportKind::Default,
                    range: extract::byte_range(alias.declarator)?,
                    local: Some(extract::byte_range(alias.name)?),
                    exported: None,
                    source: None,
                    container: None,
                    type_only: false,
                },
            };
            let recorded = if binding.local == Some(extract::byte_range(alias.name)?) {
                alias.declarator
            } else {
                write
            };
            bindings.insert(recorded.id(), binding);
        }
    }
    if current.is_empty() {
        return Ok(());
    }
    for &assignment in &captured.assignments {
        if let Some(binding) = alias_write(root, &current, assignment, source, kinds)? {
            bindings.insert(assignment.id(), binding);
        }
    }
    Ok(())
}

fn text<'source>(source: &'source str, node: Node<'_>) -> &'source str {
    source.get(node.byte_range()).unwrap_or_default()
}

/// Whether the alias still names the exported object when the module body ends.
fn alias_is_current(body: &ModuleBody<'_, '_>, alias: &Alias<'_>) -> bool {
    let Some(end) = body.replaced_until() else {
        return true;
    };
    match alias.write {
        Some(write) => write.end_byte() == end,
        // `exports` names the replaced object only after a checked synchronization.
        None => {
            end <= alias.declarator.start_byte()
                && (text(body.source, alias.value) == "module.exports" || body.synchronizes(end))
        }
    }
}

/// `app.name = value` in the module body, with `app` the checked alias. A function is
/// declared as the member the assignment writes; an identifier names its declaration.
fn alias_write(
    root: Node<'_>,
    aliases: &BTreeMap<&str, &Alias<'_>>,
    assignment: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Result<Option<SyntaxExportBinding>, RiftError> {
    let statement = assignment.parent().filter(|statement| {
        statement.kind_id() == kinds.expression_statement && statement.parent() == Some(root)
    });
    let left = kinds
        .child(assignment, EcmaScriptGrammarField::Left)
        .filter(|left| left.kind_id() == kinds.member_expression);
    let (Some(_), Some(left)) = (statement, left) else {
        return Ok(None);
    };
    let receiver = kinds
        .child(left, EcmaScriptGrammarField::Object)
        .filter(|object| object.kind_id() == kinds.identifier)
        .and_then(|object| aliases.get(text(source, object)));
    let property = kinds
        .child(left, EcmaScriptGrammarField::Property)
        .filter(|property| property.kind_id() == kinds.property_identifier);
    let value = kinds.child(assignment, EcmaScriptGrammarField::Right);
    let (Some(alias), Some(property), Some(value)) = (receiver, property, value) else {
        return Ok(None);
    };
    if assignment.start_byte() < alias.declarator.end_byte() {
        return Ok(None);
    }
    let local = if kinds
        .function_values
        .contains(&kinds.expression_value(value).kind_id())
    {
        property
    } else if value.kind_id() == kinds.identifier && !is_undefined(value, source) {
        value
    } else {
        return Ok(None);
    };
    named_binding(assignment, local, property).map(Some)
}

fn named_binding(
    range: Node<'_>,
    local: Node<'_>,
    exported: Node<'_>,
) -> Result<SyntaxExportBinding, RiftError> {
    Ok(SyntaxExportBinding {
        kind: SyntaxExportKind::Named,
        range: extract::byte_range(range)?,
        local: Some(extract::byte_range(local)?),
        exported: Some(extract::byte_range(exported)?),
        source: None,
        container: None,
        type_only: false,
    })
}

/// Records the export calls TypeScript writes into its `CommonJS` output:
/// `Object.defineProperty(exports, "name", ..)`, `__exportStar(require(".."), exports)` and
/// the older `__export(require(".."))`.
fn record_calls(
    body: &ModuleBody<'_, '_>,
    calls: &[Node<'_>],
    bindings: &mut BTreeMap<usize, SyntaxExportBinding>,
) -> Result<(), RiftError> {
    let ModuleBody { source, kinds, .. } = *body;
    for &call in calls {
        if let Some(target) = star_source(call, source) {
            // The helper copies onto `exports`, which names the exported object only until
            // `module.exports` is replaced.
            if body.replaced_until().is_some() && body.synchronized.is_none() {
                continue;
            }
            bindings.insert(
                call.id(),
                forward_binding(extract::byte_range(call)?, target)?,
            );
            continue;
        }
        let Some((receiver, name)) = defined_property(call, source, kinds) else {
            continue;
        };
        let kept = body.replaced_until().is_none_or(|end| {
            end <= call.start_byte() && (receiver == "module.exports" || body.synchronizes(end))
        });
        if let Some(value) = defined_value(call, source, kinds).filter(|_| kept) {
            bindings.insert(call.id(), named_binding(call, value, name)?);
        }
    }
    Ok(())
}

fn call_arguments(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .map(|arguments| {
            named_children(arguments)
                .filter(|argument| argument.kind() != "comment")
                .collect()
        })
        .unwrap_or_default()
}

/// The module `__exportStar(require("./module"), exports)` copies its exports from.
fn star_source<'tree>(call: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    let function = text(source, call.child_by_field_name("function")?);
    let arguments = call_arguments(call);
    let required = match arguments.as_slice() {
        [required, exports]
            if function.rsplit_once('.').map_or(function, |(_, name)| name) == "__exportStar"
                && text(source, *exports) == "exports" =>
        {
            *required
        }
        [required] if function == "__export" => *required,
        _ => return None,
    };
    required_module(required, source)
}

/// The string literal of a `require("./module")` call.
fn required_module<'tree>(value: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    if value.kind() != "call_expression"
        || text(source, value.child_by_field_name("function")?) != "require"
    {
        return None;
    }
    match call_arguments(value).as_slice() {
        [module] if module.kind() == "string" => Some(*module),
        _ => None,
    }
}

/// The receiver and the literal name of `Object.defineProperty(exports, "name", ..)`.
fn defined_property<'tree, 'source>(
    call: Node<'tree>,
    source: &'source str,
    kinds: &EcmaScriptKinds,
) -> Option<(&'source str, Node<'tree>)> {
    if text(source, call.child_by_field_name("function")?) != "Object.defineProperty" {
        return None;
    }
    let arguments = call_arguments(call);
    let [receiver, name, descriptor] = arguments.as_slice() else {
        return None;
    };
    let receiver = text(source, *receiver);
    if !matches!(receiver, "exports" | "module.exports")
        || name.kind() != "string"
        || name.named_child_count() != 1
        || descriptor.kind_id() != kinds.object_literal
    {
        return None;
    }
    let fragment = name
        .named_child(0)
        .filter(|fragment| fragment.kind() == "string_fragment")?;
    // The interop marker describes the module; it is no export of it.
    (text(source, fragment) != "__esModule").then_some((receiver, fragment))
}

/// The name or member a property descriptor gives: its `value`, or what its `get` returns.
fn defined_value<'tree>(
    call: Node<'tree>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Option<Node<'tree>> {
    let descriptor = *call_arguments(call).get(2)?;
    let mut found = None;
    for property in named_children(descriptor) {
        // A spread or computed entry can give another value.
        let key = match property.kind() {
            "pair" => property.child_by_field_name("key")?,
            "method_definition" => property.child_by_field_name("name")?,
            "comment" => continue,
            _ => return None,
        };
        if key.kind_id() != kinds.property_identifier {
            return None;
        }
        let value = match (property.kind(), text(source, key)) {
            ("method_definition", "get") => property.child_by_field_name("body").and_then(returned),
            ("pair", "get") => property
                .child_by_field_name("value")
                .filter(|value| kinds.function_values.contains(&value.kind_id()))
                .and_then(|function| function.child_by_field_name("body"))
                .and_then(returned),
            ("pair", "value") => property.child_by_field_name("value"),
            _ => None,
        };
        let value = value.filter(|value| {
            value.kind_id() == kinds.identifier || value.kind_id() == kinds.member_expression
        });
        if value.is_some() && found.replace(value).is_some() {
            return None;
        }
    }
    found.flatten()
}

/// The expression a getter body returns, when returning it is all the body does.
fn returned(body: Node<'_>) -> Option<Node<'_>> {
    if body.kind() != "statement_block" {
        return Some(body);
    }
    let mut statements = named_children(body).filter(|statement| statement.kind() != "comment");
    let statement = statements.next()?;
    (statement.kind() == "return_statement" && statements.next().is_none())
        .then(|| statement.named_child(0))
        .flatten()
}

/// `void 0` or `undefined`: the export exists and holds no declaration yet.
fn is_undefined(value: Node<'_>, source: &str) -> bool {
    matches!(text(source, value), "undefined" | "void 0")
}

#[derive(Default)]
struct CapturedAssignments<'tree> {
    assignments: Vec<Node<'tree>>,
    /// Calls that are statements of the module body.
    calls: Vec<Node<'tree>>,
    aliases: Vec<Alias<'tree>>,
    /// Every name a declaration outside a named scope binds.
    names: BTreeSet<String>,
    /// How many declarations outside a named scope bind each name.
    bound: BTreeMap<String, usize>,
    /// Names an assignment, an update or a loop gives another value.
    rebound: BTreeSet<String>,
    blocked: BTreeSet<usize>,
    replaced: BTreeMap<usize, usize>,
    synchronized: Option<(Node<'tree>, Node<'tree>)>,
    direct_default: Option<Node<'tree>>,
}

/// Captures assignments and wrapper rebindings in the bounded syntax walk.
fn capture_assignments<'tree>(
    root: Node<'tree>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    kinds: &EcmaScriptKinds,
) -> Result<CapturedAssignments<'tree>, RiftError> {
    let mut pending = vec![(root, 0_usize, true)];
    let mut captured = CapturedAssignments::default();
    let mut visited = 0_usize;
    let mut cursor = root.walk();
    while let Some((syntax_element, depth, module_scope)) = pending.pop() {
        visited += 1;
        if depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        if visited + pending.len() > limits.syntax_elements_max() {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
        captured.add(syntax_element, root, source.text, kinds);
        captured.rebind(syntax_element, source.text);
        if visited
            .saturating_add(pending.len())
            .saturating_add(syntax_element.named_child_count())
            > limits.syntax_elements_max()
        {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
        if module_scope {
            declared_names(syntax_element, source.text, kinds, &mut captured.names);
            captured.bind(syntax_element, source.text, kinds);
        }
        let module_scope =
            module_scope && !opens_named_scope(syntax_element, root, source.text, kinds);
        extract::push_named_children(&mut pending, &mut cursor, syntax_element, |child| {
            (child, depth + 1, module_scope)
        });
    }
    Ok(captured)
}

impl<'tree> CapturedAssignments<'tree> {
    /// Counts the one name a variable, function or class declaration binds.
    fn bind(&mut self, syntax_element: Node<'tree>, source: &str, kinds: &EcmaScriptKinds) {
        if matches!(
            kinds.symbol_kind(syntax_element),
            Some(
                EcmaScriptSymbolKind::Variable
                    | EcmaScriptSymbolKind::Function
                    | EcmaScriptSymbolKind::Class
            )
        ) && let Some(name) = kinds.name_syntax_element(syntax_element)
        {
            binding_holds(name, source, |identifier| {
                *self.bound.entry(identifier.to_owned()).or_default() += 1;
                false
            });
        }
    }

    /// Adds each name an assignment, an update or a loop head without a declaration writes,
    /// and a replacement of `module.exports` in a form no assignment binding reads.
    fn rebind(&mut self, syntax_element: Node<'tree>, source: &str) {
        let written = match syntax_element.kind() {
            "assignment_expression" | "augmented_assignment_expression" => {
                syntax_element.child_by_field_name("left")
            }
            "update_expression" => syntax_element.child_by_field_name("argument"),
            "for_in_statement" => syntax_element
                .child_by_field_name("left")
                .filter(|_| syntax_element.child_by_field_name("kind").is_none()),
            _ => None,
        };
        let Some(written) = written else {
            return;
        };
        let plain = syntax_element.kind() == "assignment_expression";
        let replaces = match written.kind() {
            "member_expression" => !plain && text(source, written) == "module.exports",
            "subscript_expression" => written
                .child_by_field_name("object")
                .is_some_and(|object| text(source, object) == "module"),
            _ => {
                binding_holds(written, source, |identifier| {
                    self.rebound.insert(identifier.to_owned());
                    false
                });
                false
            }
        };
        if replaces && let Some(scope) = lexical_scope(syntax_element) {
            let end = self.replaced.entry(scope.id()).or_default();
            *end = (*end).max(syntax_element.end_byte());
        }
    }

    /// Adds what one element writes: a rebound wrapper name, an assignment, a call of the
    /// module body or an alias of the exported object.
    fn add(
        &mut self,
        syntax_element: Node<'tree>,
        root: Node<'tree>,
        source: &str,
        kinds: &EcmaScriptKinds,
    ) {
        if blocked_binding(syntax_element, source)
            && let Some(scope) = lexical_scope(syntax_element)
        {
            let write = synchronization(syntax_element, root, source, kinds)
                .or_else(|| aliased_synchronization(syntax_element, root, source, kinds));
            match write {
                Some(write) => {
                    let previous = self.synchronized.replace((syntax_element, write));
                    self.blocked.extend(previous.map(|_| root.id()));
                }
                None => {
                    self.blocked.insert(scope.id());
                }
            }
        }
        let statement = || {
            syntax_element.parent().is_some_and(|statement| {
                statement.kind_id() == kinds.expression_statement
                    && statement.parent() == Some(root)
            })
        };
        if syntax_element.kind_id() == kinds.assignment_expression {
            self.assignments.push(syntax_element);
            if kinds
                .child(syntax_element, EcmaScriptGrammarField::Left)
                .is_some_and(|left| text(source, left) == "module.exports")
                && let Some(scope) = lexical_scope(syntax_element)
            {
                self.names.insert(DEFAULT_EXPORT_NAME.to_owned());
                let end = self.replaced.entry(scope.id()).or_default();
                *end = (*end).max(syntax_element.end_byte());
                if statement() {
                    self.direct_default = Some(syntax_element);
                }
            }
        } else if syntax_element.kind() == "call_expression" && statement() {
            self.calls.push(syntax_element);
        }
        self.aliases
            .extend(alias(syntax_element, root, source, kinds));
    }
}

/// Whether declarations inside `syntax_element` qualify under its name: a declaration that
/// holds others, a function assigned to an export, or the literal the module exports whole.
fn opens_named_scope(
    syntax_element: Node<'_>,
    root: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> bool {
    if syntax_element.kind_id() == kinds.assignment_expression {
        let function = kinds
            .export_definition(syntax_element, source)
            .is_some_and(|(_, value)| value.kind_id() != kinds.class_expression);
        let literal = kinds
            .child(syntax_element, EcmaScriptGrammarField::Left)
            .is_some_and(|left| kinds.is_module_exports(left, source))
            && kinds
                .child(syntax_element, EcmaScriptGrammarField::Right)
                .is_some_and(|value| value.kind_id() == kinds.object_literal)
            && syntax_element.parent().is_some_and(|statement| {
                statement.kind_id() == kinds.expression_statement
                    && statement.parent() == Some(root)
            });
        return function || literal;
    }
    kinds.symbol_kind(syntax_element).is_some_and(|kind| {
        kind.opens_scope()
            && (kind != EcmaScriptSymbolKind::Variable
                || kinds
                    .child(syntax_element, EcmaScriptGrammarField::Name)
                    .is_some_and(|name| name.kind_id() == kinds.identifier))
    })
}

/// Adds every name this element can declare at module scope. A pattern adds each
/// identifier inside it, so the set can hold more names than the file declares.
fn declared_names(
    syntax_element: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
    names: &mut BTreeSet<String>,
) {
    let name = kinds
        .symbol_kind(syntax_element)
        .and_then(|_| kinds.name_syntax_element(syntax_element))
        .or_else(|| match syntax_element.kind() {
            "catch_clause" => syntax_element.child_by_field_name("parameter"),
            "for_in_statement" => syntax_element.child_by_field_name("left"),
            _ => None,
        });
    if let Some(name) = name {
        binding_holds(name, source, |identifier| {
            names.insert(identifier.to_owned());
            false
        });
        names.insert(text(source, name).to_owned());
    }
}

/// Finds the authored bindings that supply these exports directly from the module body.
fn initialized_exports(
    root: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
    bindings: &BTreeMap<usize, SyntaxExportBinding>,
) -> Result<Vec<crate::ByteRange>, RiftError> {
    let exported = bindings
        .values()
        .filter(|binding| binding.container.is_none() && binding.source.is_none())
        .filter_map(|binding| {
            let range = binding.local?;
            source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
        })
        .collect::<BTreeSet<_>>();
    let mut initialized = Vec::new();
    let mut record = |declaration| -> Result<(), RiftError> {
        if let Some(name) = kinds.child(declaration, EcmaScriptGrammarField::Name)
            && name.kind_id() == kinds.identifier
            && source
                .get(name.byte_range())
                .is_some_and(|name| exported.contains(name))
        {
            initialized.push(extract::byte_range(name)?);
        }
        Ok(())
    };
    // The main capture walk bounded this tree. Only direct children can run
    // unconditionally in the module, regardless of their lexical hoisting scope.
    for statement in named_children(root) {
        match statement.kind() {
            "function_declaration" => record(statement)?,
            "variable_declaration" | "lexical_declaration" => {
                for declaration in named_children(statement) {
                    if declaration.kind_id() == kinds.variable_declarator
                        && kinds
                            .child(declaration, EcmaScriptGrammarField::Value)
                            .is_some()
                    {
                        record(declaration)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(initialized)
}

/// Inner write of one unconditional `exports = module.exports = value` statement.
fn synchronization<'tree>(
    outer: Node<'tree>,
    root: Node<'tree>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Option<Node<'tree>> {
    if outer.kind_id() != kinds.assignment_expression {
        return None;
    }
    let left = kinds.child(outer, EcmaScriptGrammarField::Left)?;
    if source.get(left.byte_range())? != "exports" {
        return None;
    }
    let statement = outer.parent()?;
    if outer.has_error()
        || statement.kind_id() != kinds.expression_statement
        || statement.parent() != Some(root)
    {
        return None;
    }
    let inner = kinds.child(outer, EcmaScriptGrammarField::Right)?;
    if inner.kind_id() != kinds.assignment_expression {
        return None;
    }
    let module = kinds.child(inner, EcmaScriptGrammarField::Left)?;
    let value = kinds.child(inner, EcmaScriptGrammarField::Right)?;
    (source.get(module.byte_range())? == "module.exports"
        && value.kind_id() != kinds.assignment_expression)
        .then_some(inner)
}

/// The `module.exports` write of `var app = exports = module.exports = value`, when `outer`
/// is its `exports` write.
fn aliased_synchronization<'tree>(
    outer: Node<'tree>,
    root: Node<'tree>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Option<Node<'tree>> {
    if outer.kind_id() != kinds.assignment_expression
        || text(source, kinds.child(outer, EcmaScriptGrammarField::Left)?) != "exports"
    {
        return None;
    }
    let mut chain = outer;
    while let Some(parent) = chain
        .parent()
        .filter(|parent| parent.kind_id() == kinds.assignment_expression)
    {
        chain = parent;
    }
    alias(chain.parent()?, root, source, kinds)?.write
}

/// The wrapper names stay inside the synchronized writes and direct root export writes.
fn closed_wrapper_uses(
    root: Node<'_>,
    outer: Option<Node<'_>>,
    inner: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> bool {
    let outer_left = outer.and_then(|outer| kinds.child(outer, EcmaScriptGrammarField::Left));
    let module = kinds
        .child(inner, EcmaScriptGrammarField::Left)
        .and_then(|left| kinds.child(left, EcmaScriptGrammarField::Object));
    let mut pending = vec![root];
    let mut cursor = root.walk();
    // The capture walk has already admitted the element count and depth of this tree.
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ) && source.get(node.byte_range()).is_some_and(|name| {
            name.contains('\\')
                || (matches!(name, "module" | "exports" | "eval")
                    && Some(node) != outer_left
                    && Some(node) != module
                    && !(outer.is_some() && direct_exports_write(node, root, source, kinds)))
        }) {
            return false;
        }
        extract::push_named_children(&mut pending, &mut cursor, node, |child| child);
    }
    true
}

fn direct_exports_write(
    receiver: Node<'_>,
    root: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> bool {
    // `exports` and `module.exports` name the same object while both stay synchronized.
    let object = match source.get(receiver.byte_range()) {
        Some("exports") => Some(receiver),
        Some("module") => receiver
            .parent()
            .filter(|exports| kinds.is_module_exports(*exports, source)),
        _ => None,
    };
    let Some(object) = object else {
        return false;
    };
    object.parent().is_some_and(|member| {
        member.kind_id() == kinds.member_expression
            && kinds.child(member, EcmaScriptGrammarField::Object) == Some(object)
            && kinds
                .child(member, EcmaScriptGrammarField::Property)
                .is_some_and(|name| name.kind_id() == kinds.property_identifier)
            && member.parent().is_some_and(|assignment| {
                assignment.kind_id() == kinds.assignment_expression
                    && !assignment.has_error()
                    && kinds.child(assignment, EcmaScriptGrammarField::Left) == Some(member)
                    && module_statement(assignment, root, kinds)
            })
    })
}

/// Whether a named export survives every `module.exports = value` of the scopes around it.
///
/// A replacement written later replaces the object the name was added to. One written
/// earlier is the object `module.exports.name = ...` adds to. A checked synchronization
/// also establishes that `exports` names that same object. The default slot stays ambiguous.
fn kept_after_replacement(
    syntax_element: Node<'_>,
    source: &str,
    replaced: &BTreeMap<usize, usize>,
    synchronized: Option<(usize, usize)>,
) -> bool {
    let left = syntax_element
        .child_by_field_name("left")
        .and_then(|left| source.get(left.byte_range()))
        .unwrap_or_default();
    let through_module = left.starts_with("module.exports.") && left != "module.exports.default";
    let mut scope = syntax_element;
    while let Some(parent) = scope.parent() {
        if let Some(&end) = replaced.get(&parent.id()) {
            let through_synchronized = synchronized == Some((parent.id(), end))
                && left.starts_with("exports.")
                && left != "exports.default";
            if end > syntax_element.start_byte() || !(through_module || through_synchronized) {
                return false;
            }
        }
        scope = parent;
    }
    true
}

fn receiver(text: &str) -> bool {
    matches!(text, "exports" | "module" | "require" | "eval")
}

fn binding_contains(syntax_element: Node<'_>, source: &str) -> bool {
    binding_holds(syntax_element, source, receiver)
}

fn binding_holds(
    syntax_element: Node<'_>,
    source: &str,
    mut matches: impl FnMut(&str) -> bool,
) -> bool {
    let mut pending = vec![syntax_element];
    let mut cursor = syntax_element.walk();
    while let Some(syntax_element) = pending.pop() {
        if matches!(
            syntax_element.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) && source
            .get(syntax_element.byte_range())
            .is_some_and(&mut matches)
        {
            return true;
        }
        extract::push_named_children(&mut pending, &mut cursor, syntax_element, |child| child);
    }
    false
}

fn blocked_binding(syntax_element: Node<'_>, source: &str) -> bool {
    let binding = match syntax_element.kind() {
        "variable_declarator" | "function_declaration" | "class_declaration" => {
            syntax_element.child_by_field_name("name")
        }
        "formal_parameters" | "import_clause" | "catch_clause" => Some(syntax_element),
        "assignment_expression" | "augmented_assignment_expression" => syntax_element
            .child_by_field_name("left")
            .filter(|left| !matches!(left.kind(), "member_expression" | "subscript_expression")),
        "update_expression" => syntax_element
            .child_by_field_name("argument")
            .filter(|left| !matches!(left.kind(), "member_expression" | "subscript_expression")),
        "call_expression" => {
            return syntax_element
                .child_by_field_name("function")
                .and_then(|function| source.get(function.byte_range()))
                == Some("eval");
        }
        _ => None,
    };
    binding.is_some_and(|binding| binding_contains(binding, source))
}

fn lexical_scope(mut syntax_element: Node<'_>) -> Option<Node<'_>> {
    let function_binding = matches!(
        syntax_element.kind(),
        "formal_parameters"
            | "variable_declarator"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "update_expression"
            | "call_expression"
            | "function_declaration"
    );
    while let Some(parent) = syntax_element.parent() {
        if matches!(
            parent.kind(),
            "program"
                | "function_expression"
                | "function_declaration"
                | "arrow_function"
                | "generator_function"
                | "generator_function_declaration"
        ) || (!function_binding && parent.kind() == "statement_block")
        {
            return Some(parent);
        }
        syntax_element = parent;
    }
    None
}

fn immediate_function(function: Node<'_>) -> bool {
    if function.kind() != "function_expression"
        || function.child_by_field_name("name").is_some()
        || function
            .child_by_field_name("parameters")
            .is_none_or(|parameters| parameters.named_child_count() != 0)
    {
        return false;
    }
    let mut called = function;
    while let Some(parent) = called.parent().filter(|parent| {
        parent.kind() == "parenthesized_expression" && parent.named_child_count() == 1
    }) {
        called = parent;
    }
    called.parent().is_some_and(|call| {
        call.kind() == "call_expression"
            && call.child_by_field_name("function") == Some(called)
            && call
                .child_by_field_name("arguments")
                .is_some_and(|arguments| arguments.named_child_count() == 0)
    })
}

fn module_execution(mut syntax_element: Node<'_>, blocked: &BTreeSet<usize>) -> bool {
    while let Some(parent) = syntax_element.parent() {
        if blocked.contains(&parent.id()) {
            return false;
        }
        match parent.kind() {
            "program" => return true,
            "function_expression" if immediate_function(parent) => {}
            "expression_statement"
            | "statement_block"
            | "if_statement"
            | "else_clause"
            | "binary_expression"
            | "parenthesized_expression"
            | "call_expression"
            | "assignment_expression" => {}
            _ => return false,
        }
        syntax_element = parent;
    }
    false
}

fn assignment(
    syntax_element: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Result<Option<SyntaxExportBinding>, RiftError> {
    let Some(left) = kinds.child(syntax_element, EcmaScriptGrammarField::Left) else {
        return Ok(None);
    };
    let Some(target) = kinds.export_target(left, source) else {
        return Ok(None);
    };
    let range = extract::byte_range(syntax_element)?;
    if let Some((property, function)) = kinds.export_definition(syntax_element, source) {
        // A named function assigned to `module.exports` is declared under its own name.
        let written = (target == ExportTarget::Module)
            .then(|| kinds.child(function, EcmaScriptGrammarField::Name))
            .flatten();
        let name = extract::byte_range(written.unwrap_or(property))?;
        return Ok(Some(SyntaxExportBinding {
            kind: if target == ExportTarget::Property {
                SyntaxExportKind::Named
            } else {
                SyntaxExportKind::Default
            },
            range,
            local: Some(name),
            exported: (target == ExportTarget::Property).then_some(name),
            source: None,
            container: None,
            type_only: false,
        }));
    }
    if target == ExportTarget::Property {
        return property_export(syntax_element, left, source, kinds);
    }
    let Some(value) = kinds.child(syntax_element, EcmaScriptGrammarField::Right) else {
        return Ok(None);
    };
    if value.kind_id() == kinds.identifier {
        // `module.exports = name` exports the declaration the name binds, as
        // `export default name` does.
        return Ok(Some(SyntaxExportBinding {
            kind: SyntaxExportKind::Default,
            range,
            local: Some(extract::byte_range(value)?),
            exported: None,
            source: None,
            container: None,
            type_only: false,
        }));
    }
    if let Some(module) = required_module(value, source) {
        return Ok(Some(forward_binding(range, module)?));
    }
    // `module.exports = require("./module").name` exports that export of the module.
    let property = kinds
        .child(value, EcmaScriptGrammarField::Property)
        .filter(|property| {
            value.kind_id() == kinds.member_expression
                && property.kind_id() == kinds.property_identifier
        });
    let module = kinds
        .child(value, EcmaScriptGrammarField::Object)
        .and_then(|object| required_module(object, source));
    let (Some(property), Some(module)) = (property, module) else {
        return Ok(None);
    };
    Ok(Some(SyntaxExportBinding {
        kind: SyntaxExportKind::Default,
        local: Some(extract::byte_range(property)?),
        ..forward_binding(range, module)?
    }))
}

/// All exports of a required module.
fn forward_binding(
    range: crate::ByteRange,
    module: Node<'_>,
) -> Result<SyntaxExportBinding, RiftError> {
    Ok(SyntaxExportBinding {
        kind: SyntaxExportKind::All,
        range,
        local: None,
        exported: None,
        source: Some(extract::byte_range(module)?),
        container: None,
        type_only: false,
    })
}

/// `module.exports = condition ? require("./a") : require("./b")`: one forward for each
/// module, over one assignment.
fn record_conditional_forward(
    assignment: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
    bindings: &mut BTreeMap<usize, SyntaxExportBinding>,
) -> Result<(), RiftError> {
    let left = kinds.child(assignment, EcmaScriptGrammarField::Left);
    if left.and_then(|left| kinds.export_target(left, source)) != Some(ExportTarget::Module) {
        return Ok(());
    }
    let Some(value) = kinds
        .child(assignment, EcmaScriptGrammarField::Right)
        .map(|value| kinds.expression_value(value))
        .filter(|value| value.kind() == "ternary_expression")
    else {
        return Ok(());
    };
    let branches = ["consequence", "alternative"].map(|field| {
        value
            .child_by_field_name(field)
            .map(|branch| kinds.expression_value(branch))
            .and_then(|branch| Some((branch, required_module(branch, source)?)))
    });
    if let [Some(first), Some(second)] = branches {
        for (branch, module) in [first, second] {
            bindings.insert(
                branch.id(),
                forward_binding(extract::byte_range(assignment)?, module)?,
            );
        }
    }
    Ok(())
}

/// `exports.name = value`: an identifier exports the declaration it names, a chain ending
/// in a function or class the declaration of its inner assignment, and any other value the
/// property the assignment itself declares.
fn property_export(
    syntax_element: Node<'_>,
    left: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Result<Option<SyntaxExportBinding>, RiftError> {
    let property = kinds
        .child(left, EcmaScriptGrammarField::Property)
        .filter(|property| property.kind_id() == kinds.property_identifier);
    let (Some(property), Some((inner, value))) = (property, kinds.assignment_chain(syntax_element))
    else {
        return Ok(None);
    };
    let local = if is_undefined(value, source) {
        None
    } else if value.kind_id() == kinds.identifier {
        Some(value)
    } else if let Some((declared, _)) = kinds.export_definition(inner, source) {
        Some(declared)
    } else {
        Some(property)
    };
    local
        .map(|local| named_binding(syntax_element, local, property))
        .transpose()
}

/// Whether the assignment declares a class or another value under a name a declaration at
/// module scope takes: `var etag = require('etag'); exports.etag = create();` writes two.
/// A function keeps the name it is assigned to.
fn declares_taken_name(
    assignment: Node<'_>,
    binding: &SyntaxExportBinding,
    source: &str,
    kinds: &EcmaScriptKinds,
    names: &BTreeSet<String>,
) -> bool {
    let function = kinds
        .export_definition(assignment, source)
        .is_some_and(|(_, value)| value.kind_id() != kinds.class_expression);
    binding.kind == SyntaxExportKind::Named
        && binding.local == binding.exported
        && !function
        && kinds
            .child(assignment, EcmaScriptGrammarField::Left)
            .and_then(|left| kinds.child(left, EcmaScriptGrammarField::Property))
            .is_some_and(|property| names.contains(text(source, property)))
}
