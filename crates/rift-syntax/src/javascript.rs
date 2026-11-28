//! JavaScript syntax facts from the pinned tree-sitter-javascript grammar.
//!
//! The grammar parses JSX, so this provider claims both `js` and `jsx`.
//! Declaration rules live in [`crate::ecmascript`], shared with the
//! TypeScript providers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use rift_protocol::read::{Language, SyntaxKind};

use crate::ByteRange;
use crate::document::SyntaxDocument;
use crate::ecmascript::{self, EcmaScriptKinds};
use crate::failure::RiftError;
use crate::provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};

/// Bounded Tree-sitter JavaScript fact provider.
#[derive(Debug, Clone)]
pub struct JavaScriptSyntaxProvider {
    language: Language,
}

impl Default for JavaScriptSyntaxProvider {
    fn default() -> Self {
        Self {
            language: Language {
                name: "javascript".to_owned(),
                dialect: None,
            },
        }
    }
}

impl JavaScriptSyntaxProvider {
    /// Proves a direct `CommonJS` export assignment follows a literal placeholder for its key.
    ///
    /// The caller supplies the exact later assignment and earlier member spans after checking
    /// all writes. The result names only complete, unconditional later assignments.
    #[must_use]
    pub fn commonjs_class_export_initializations(
        source: &str,
        limits: SyntaxLimits,
        initializations: &BTreeMap<ByteRange, ByteRange>,
    ) -> BTreeSet<ByteRange> {
        ecmascript::commonjs_class_export_initializations(source, limits, initializations)
    }

    /// Checks literal properties written in direct arguments of a JavaScript module-body call.
    ///
    /// The statement, call, argument, and property must have independent exact syntax.
    /// Nested objects, arrays, computed keys, and incomplete units produce no proof.
    #[must_use]
    pub fn module_call_argument_properties(
        source: &str,
        limits: SyntaxLimits,
        selected: &BTreeSet<ByteRange>,
    ) -> BTreeSet<ByteRange> {
        Self::scoped_call_argument_properties(source, limits, selected, &BTreeMap::new())
    }

    /// Checks written zero-argument arrow properties of direct module call arguments.
    #[must_use]
    pub fn module_arrow_argument_properties(
        source: &str,
        limits: SyntaxLimits,
        selected: &BTreeSet<ByteRange>,
    ) -> BTreeSet<ByteRange> {
        module_object_members(source, limits, selected.len(), |root, node, range, _| {
            selected.contains(&range) && direct_arrow_getter(root, node)
        })
    }

    /// Checks direct argument properties in complete module and function bodies.
    ///
    /// A function case requires the property and its independent class or variable
    /// declaration to be direct statements in the same body.
    #[must_use]
    pub fn scoped_call_argument_properties(
        source: &str,
        limits: SyntaxLimits,
        module: &BTreeSet<ByteRange>,
        functions: &BTreeMap<ByteRange, ByteRange>,
    ) -> BTreeSet<ByteRange> {
        let selected = module.len().saturating_add(functions.len());
        module_object_members(source, limits, selected, |root, property, range, _| {
            if property.kind() != "pair"
                || property
                    .child_by_field_name("key")
                    .is_none_or(|key| key.kind() != "property_identifier")
            {
                return false;
            }
            if module.contains(&range) {
                return direct_argument_statement(property)
                    .and_then(|statement| statement.parent())
                    == Some(root);
            }
            functions
                .get(&range)
                .is_some_and(|binding| sibling_argument_binding(root, property, *binding))
        })
    }

    /// Checks getter slots in direct module call arguments and keys of short-circuited
    /// `CommonJS` export objects. These keys occupy an object scope.
    #[must_use]
    pub fn module_disjoint_object_members(
        source: &str,
        limits: SyntaxLimits,
        selected: &BTreeSet<ByteRange>,
    ) -> BTreeSet<ByteRange> {
        module_object_members(
            source,
            limits,
            selected.len(),
            |root, node, range, source| {
                if !selected.contains(&range) {
                    return false;
                }
                match node.kind() {
                    "method_definition" => {
                        let mut cursor = node.walk();
                        node.children(&mut cursor)
                            .find(|child| !child.is_extra())
                            .is_some_and(|token| token.kind() == "get")
                            && node
                                .child_by_field_name("name")
                                .is_some_and(|key| key.kind() == "property_identifier")
                            && node
                                .child_by_field_name("parameters")
                                .is_some_and(|parameters| {
                                    let mut cursor = parameters.walk();
                                    parameters
                                        .named_children(&mut cursor)
                                        .all(|formal| formal.is_extra())
                                })
                            && direct_argument_statement(node)
                                .and_then(|statement| statement.parent())
                                == Some(root)
                    }
                    "pair" => {
                        node.child_by_field_name("key")
                            .is_some_and(|key| key.kind() == "property_identifier")
                            && (short_circuited_export_member(node, source)
                                || direct_arrow_getter(root, node))
                    }
                    "shorthand_property_identifier" => short_circuited_export_member(node, source),
                    _ => false,
                }
            },
        )
    }
}

fn module_object_members(
    source: &str,
    limits: SyntaxLimits,
    selected: usize,
    accepts: impl for<'tree> Fn(
        tree_sitter::Node<'tree>,
        tree_sitter::Node<'tree>,
        ByteRange,
        &str,
    ) -> bool,
) -> BTreeSet<ByteRange> {
    if selected == 0
        || selected > limits.syntax_elements_max()
        || source.len() > limits.source_bytes_max()
    {
        return BTreeSet::new();
    }
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&javascript_grammar()).is_err() {
        return BTreeSet::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return BTreeSet::new();
    };
    let root = tree.root_node();
    if root.has_error() {
        return BTreeSet::new();
    }
    let mut visited = 0_usize;
    let mut pending = vec![(root, 0_usize)];
    let mut cursor = root.walk();
    let mut accepted = BTreeSet::new();
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        if depth > limits.syntax_depth_max()
            || visited
                .saturating_add(pending.len())
                .saturating_add(node.named_child_count())
                > limits.syntax_elements_max()
        {
            return BTreeSet::new();
        }
        if matches!(
            node.kind(),
            "pair" | "method_definition" | "shorthand_property_identifier"
        ) && let Ok(range) = crate::extract::byte_range(node)
            && accepts(root, node, range, source)
        {
            accepted.insert(range);
        }
        crate::extract::push_named_children(&mut pending, &mut cursor, node, |child| {
            (child, depth + 1)
        });
    }
    accepted
}

fn direct_arrow_getter(root: tree_sitter::Node<'_>, pair: tree_sitter::Node<'_>) -> bool {
    let Some(arrow) = pair
        .child_by_field_name("value")
        .filter(|value| value.kind() == "arrow_function")
    else {
        return false;
    };
    arrow
        .child_by_field_name("body")
        .is_some_and(|body| body.kind() == "identifier")
        && arrow
            .child_by_field_name("parameters")
            .is_some_and(|formals| {
                let mut cursor = formals.walk();
                formals
                    .named_children(&mut cursor)
                    .all(|formal| formal.is_extra())
            })
        && direct_argument_statement(pair).and_then(|statement| statement.parent()) == Some(root)
}

fn direct_argument_statement(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let object = node.parent().filter(|parent| parent.kind() == "object")?;
    let arguments = object
        .parent()
        .filter(|parent| parent.kind() == "arguments")?;
    let call = arguments
        .parent()
        .filter(|parent| parent.kind() == "call_expression")?;
    if call
        .child_by_field_name("function")
        .is_none_or(|function| function.kind() != "identifier")
        || call.child_by_field_name("arguments") != Some(arguments)
        || call.child_by_field_name("optional_chain").is_some()
    {
        return None;
    }
    call.parent()
        .filter(|statement| statement.kind() == "expression_statement")
}

fn sibling_argument_binding(
    root: tree_sitter::Node<'_>,
    property: tree_sitter::Node<'_>,
    range: ByteRange,
) -> bool {
    let body = direct_argument_statement(property).and_then(|node| node.parent());
    let Some(body) = body.filter(|body| {
        body.kind() == "statement_block"
            && body.parent().is_some_and(|function| {
                matches!(
                    function.kind(),
                    "arrow_function" | "function_expression" | "function_declaration"
                ) && function.child_by_field_name("body") == Some(*body)
            })
    }) else {
        return false;
    };
    let Some(binding) = usize::try_from(range.start)
        .ok()
        .zip(usize::try_from(range.end).ok())
        .and_then(|(start, end)| root.named_descendant_for_byte_range(start, end))
        .filter(|binding| crate::extract::byte_range(*binding).ok() == Some(range))
    else {
        return false;
    };
    let statement = match binding.kind() {
        "class_declaration" => Some(binding),
        "variable_declarator" => binding
            .parent()
            .filter(|node| matches!(node.kind(), "variable_declaration" | "lexical_declaration")),
        _ => None,
    };
    statement.and_then(|statement| statement.parent()) == Some(body)
}

fn short_circuited_export_member(node: tree_sitter::Node<'_>, source: &str) -> bool {
    let Some(object) = node.parent().filter(|parent| parent.kind() == "object") else {
        return false;
    };
    let Some(assignment) = object.parent().filter(|parent| {
        parent.kind() == "assignment_expression"
            && parent.child_by_field_name("right") == Some(object)
    }) else {
        return false;
    };
    let Some(target) = assignment.child_by_field_name("left") else {
        return false;
    };
    if target.kind() != "member_expression"
        || target.child_by_field_name("optional_chain").is_some()
        || target.child_by_field_name("object").is_none_or(|receiver| {
            receiver.kind() != "identifier" || source.get(receiver.byte_range()) != Some("module")
        })
        || target
            .child_by_field_name("property")
            .is_none_or(|property| {
                property.kind() != "property_identifier"
                    || source.get(property.byte_range()) != Some("exports")
            })
    {
        return false;
    }
    let Some(wrapped) = assignment
        .parent()
        .filter(|parent| parent.kind() == "parenthesized_expression")
    else {
        return false;
    };
    let Some(guard) = wrapped.parent().filter(|parent| {
        parent.kind() == "binary_expression"
            && parent.child_by_field_name("right") == Some(wrapped)
            && parent
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "&&")
    }) else {
        return false;
    };
    guard.child_by_field_name("left").is_some_and(|condition| {
        condition.kind() == "false"
            || (condition.kind() == "number" && source.get(condition.byte_range()) == Some("0"))
    }) && guard.parent().is_some_and(|statement| {
        statement.kind() == "expression_statement"
            && statement
                .parent()
                .is_some_and(|root| root.kind() == "program")
    })
}

impl SyntaxProvider for JavaScriptSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        ecmascript::analyze(
            &self.language,
            &javascript_grammar(),
            javascript_kinds(),
            limits,
            source,
        )
    }

    fn syntax_element_kinds(&self, kind: &str) -> Vec<SyntaxKind> {
        ecmascript::syntax_element_kinds(kind)
    }
}

pub(crate) fn javascript_grammar() -> tree_sitter::Language {
    tree_sitter_javascript::LANGUAGE.into()
}

/// Returns the process-wide resolved JavaScript kind table, computing it
/// once.
pub(crate) fn javascript_kinds() -> &'static EcmaScriptKinds {
    static KINDS: OnceLock<EcmaScriptKinds> = OnceLock::new();
    KINDS.get_or_init(|| EcmaScriptKinds::resolve_javascript(&javascript_grammar()))
}

#[cfg(test)]
mod tests {
    use rift_core::{PROVIDER_SYMBOL_ID_BYTES_MAX, ProjectPath};
    use rift_protocol::read::SymbolKind;

    use super::*;

    fn path() -> ProjectPath {
        ProjectPath::new("src/app.js").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        JavaScriptSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text,
                },
                SyntaxLimits::default(),
            )
            .expect("JavaScript fixture must parse")
    }

    #[test]
    fn test_provider_declares_language() {
        let provider = JavaScriptSyntaxProvider::default();
        assert_eq!(provider.language().name, "javascript");
        assert_eq!(provider.language().dialect, None);
    }

    #[test]
    fn test_document_kind_words_cover_every_javascript_declaration_kind() {
        let text = "function plain() {}\nfunction* pages() {}\nclass Router {\n  route(path) {}\n}\nconst limit = 3;\nvar legacy = 1;\n";
        let document = analyze(text);
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                ("plain", "function"),
                ("pages", "function"),
                ("Router", "class"),
                ("route", "method"),
                ("path", "method_parameter"),
                ("limit", "variable"),
                ("legacy", "variable"),
            ]
        );
        assert!(!document.has_errors());
    }

    #[test]
    fn test_class_bodies_nest_methods_under_the_class_qualified_name() {
        let text = "class Router {\n  static of(kind) {}\n  #secret() {}\n  route(path) {}\n}\n";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.container.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("Router", None),
                ("Router.of", Some("Router")),
                ("Router.of.kind", Some("Router.of")),
                ("Router.#secret", Some("Router")),
                ("Router.route", Some("Router")),
                ("Router.route.path", Some("Router.route")),
            ]
        );
    }

    /// An arrow function assigned to a declarator is that variable, named by
    /// the declarator; its value span is the body range.
    #[test]
    fn test_arrow_function_declarator_emits_a_variable_named_by_the_declarator() {
        let text = "const render = (value) => value;\n";
        let document = analyze(text);
        let symbol = &document.symbols()[0];
        assert_eq!(symbol.name, "render");
        assert_eq!(symbol.kind, "variable");
        let body = symbol.body_range.expect("the declarator holds a value");
        let start = usize::try_from(body.start).expect("fixture span fits usize");
        let end = usize::try_from(body.end).expect("fixture span fits usize");
        assert_eq!(&text[start..end], "(value) => value");
    }

    /// A destructuring declarator declares each name its pattern binds, and
    /// no symbol for the pattern itself.
    #[test]
    fn test_destructuring_declarator_declares_each_bound_name() {
        let document = analyze("const { first, second } = pair;\nconst kept = 1;\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["first", "second", "kept"]);
    }

    #[test]
    fn test_exported_declarations_carry_the_public_kind_and_no_visibility() {
        let text = "export function shipped() {}\nexport const limit = 1;\nexport default class Router {}\nfunction hidden() {}\n";
        let document = analyze(text);
        let facts = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.kinds.contains(&SymbolKind::Public),
                    symbol.visibility.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            facts,
            [
                ("shipped", true, None),
                ("limit", true, None),
                ("Router", true, None),
                ("hidden", false, None),
            ]
        );
    }

    /// Every qualified name `document` declares, beside whether it carries the
    /// `Public` kind.
    fn public_facts(document: &SyntaxDocument) -> Vec<(&str, bool)> {
        document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.kinds.contains(&SymbolKind::Public),
                )
            })
            .collect()
    }

    /// An export clause marks the module-scope declarations it names, an
    /// aliased one under its own name, and leaves the rest unmarked.
    #[test]
    fn test_an_export_clause_marks_the_local_declarations_it_names() {
        let document = analyze(
            "function open() {}\nconst limit = 1, spare = 2;\nclass Router {\n  route() {}\n}\nfunction hidden() {}\nexport { open, limit as max, Router as default };\n",
        );
        assert_eq!(
            public_facts(&document),
            [
                ("open", true),
                ("limit", true),
                ("spare", false),
                ("Router", true),
                ("Router.route", false),
                ("hidden", false),
            ]
        );
    }

    /// `export default` of an identifier marks the declaration it names; of
    /// an object literal, the declarations its properties name and the
    /// methods written in it.
    #[test]
    fn test_a_default_export_marks_the_declarations_it_names() {
        let document = analyze("function main() {}\nfunction helper() {}\nexport default main;\n");
        assert_eq!(public_facts(&document), [("main", true), ("helper", false)]);

        let document = analyze(
            "const answer = 1;\nfunction helper() {}\nfunction hidden() {}\nexport default { answer, run: helper, start() {} };\n",
        );
        assert_eq!(
            public_facts(&document),
            [
                ("answer", true),
                ("helper", true),
                ("hidden", false),
                ("default", true),
                ("default.answer", true),
                ("default.run", true),
                ("default.start", true),
            ]
        );
    }

    /// A whole export of a computed value, such as a call's result or a
    /// number, names no declaration, so it marks none.
    #[test]
    fn test_a_whole_export_of_a_computed_value_marks_nothing() {
        let document = analyze("function create() {}\nexport default create();\n");
        assert_eq!(public_facts(&document), [("create", false)]);

        let document = analyze("function create() {}\nmodule.exports = create();\n");
        assert_eq!(public_facts(&document), [("create", false)]);

        let document = analyze("const answer = 1;\nexport default 42;\n");
        assert_eq!(public_facts(&document), [("answer", false)]);
    }

    /// A re-export names another module's declarations, so a local
    /// declaration of the same name stays unmarked.
    #[test]
    fn test_a_re_export_marks_no_local_declaration() {
        let document = analyze(
            "function open() {}\nexport { open } from './open.js';\nexport * from './more.js';\nexport * as tools from './tools.js';\n",
        );
        assert_eq!(public_facts(&document), [("open", false)]);
    }

    /// An export names a module-scope declaration: a nested declaration or a
    /// class member sharing its name stays unmarked.
    #[test]
    fn test_an_exported_name_marks_only_the_module_scope_declaration() {
        let document = analyze(
            "function open() {}\nfunction outer() {\n  function open() {}\n  const limit = 2;\n}\nclass Router {\n  open() {}\n}\nconst limit = 1;\nexport { open, limit };\n",
        );
        assert_eq!(
            public_facts(&document),
            [
                ("open", true),
                ("outer", false),
                ("outer.open", false),
                ("outer.limit", false),
                ("Router", false),
                ("Router.open", false),
                ("limit", true),
            ]
        );
    }

    /// `module.exports = { a, b: c }` marks `a`, `c`, and the methods written
    /// in the object; `module.exports = X` marks `X`.
    #[test]
    fn test_a_module_exports_assignment_marks_the_declarations_it_names() {
        let document = analyze(
            "function helper() {}\nfunction start() {}\nfunction hidden() {}\nmodule.exports = { helper, run: start, stop() {}, ...hidden };\n",
        );
        assert_eq!(
            public_facts(&document),
            [
                ("helper", true),
                ("start", true),
                ("hidden", false),
                ("default", true),
                ("default.helper", true),
                ("default.run", true),
                ("default.stop", true),
            ]
        );

        let document = analyze(
            "class Runner {\n  run() {}\n}\nfunction hidden() {}\nmodule.exports = Runner;\n",
        );
        assert_eq!(
            public_facts(&document),
            [("Runner", true), ("Runner.run", false), ("hidden", false)]
        );
    }

    /// A property export marks the declaration its value names:
    /// `module.exports.a = a`, `exports.a = a`, and `exports.b = c`.
    #[test]
    fn test_a_property_export_marks_the_declaration_its_value_names() {
        let document = analyze(
            "function parse() {}\nfunction load() {}\nconst start = () => 1;\nfunction hidden() {}\nmodule.exports.parse = parse;\nexports.load = load;\nexports.run = start;\n",
        );
        assert_eq!(
            public_facts(&document),
            [
                ("parse", true),
                ("load", true),
                ("start", true),
                ("hidden", false),
            ]
        );
    }

    /// An assignment chain exports its value through the widest export target
    /// it passes, as `exports = module.exports = create` does.
    #[test]
    fn test_an_assignment_chain_exports_through_its_widest_target() {
        let document = analyze("function create() {}\nexports = module.exports = create;\n");
        assert_eq!(public_facts(&document), [("create", true)]);

        let document = analyze("function helper() {}\nmodule.exports = exports = { helper };\n");
        assert_eq!(
            public_facts(&document),
            [
                ("helper", true),
                ("default", true),
                ("default.helper", true)
            ]
        );
    }

    /// A rebound `exports` leaves every property assignment without export context.
    #[test]
    fn test_an_export_assignment_marks_nothing_it_does_not_name() {
        let document = analyze(
            "function helper() {}\nconst config = { load() {} };\nexports.extra = function extra() {};\nexports.Kind = class Kind {};\nexports.parse = config.parse;\nexports.config = { load() {} };\nexports = helper;\nmodule.helper = helper;\nif (typeof module === 'object') {\n  module.exports = helper;\n}\n",
        );
        assert_eq!(
            public_facts(&document),
            [
                ("helper", false),
                ("config", false),
                ("config.load", false),
                ("extra", false),
                ("load", false),
            ]
        );
    }

    #[test]
    fn test_document_kinds_render_kind_categories() {
        let document = analyze(
            "export function shipped() {}\nclass Router { route() {} }\nconst limit = 1;\n",
        );
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| symbol.kinds.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                vec![SymbolKind::Value, SymbolKind::Callable, SymbolKind::Public],
                vec![SymbolKind::Type],
                vec![SymbolKind::Value, SymbolKind::Callable],
                vec![SymbolKind::Value],
            ]
        );
    }

    /// Body ranges span the statement block, class body, or declarator
    /// value, and stay absent where the syntax element omits the field.
    #[test]
    fn test_body_range_present_exactly_where_the_syntax_element_declares_one() {
        let text = "function compute() { return 1; }\nclass Router { route() {} }\nlet bare;\nconst limit = 3;\n";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.body_range.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("compute", true),
                ("Router", true),
                ("route", true),
                ("bare", false),
                ("limit", true),
            ]
        );
        let compute = &document.symbols()[0];
        let body = compute.body_range.expect("compute owns a body");
        let start = usize::try_from(body.start).expect("fixture span fits usize");
        let end = usize::try_from(body.end).expect("fixture span fits usize");
        assert_eq!(&text[start..end], "{ return 1; }");
    }

    /// The text `range` spans in `text`.
    fn spanned(text: &str, range: crate::document::ByteRange) -> &str {
        let start = usize::try_from(range.start).expect("fixture span fits usize");
        let end = usize::try_from(range.end).expect("fixture span fits usize");
        &text[start..end]
    }

    /// An exported declaration's complete span starts at `export`, the
    /// statement carrying it, while `item_range` stays the declaration's own
    /// syntax element; a plain comment before the statement stays outside both.
    #[test]
    fn test_declaration_range_starts_at_the_statement_carrying_it() {
        let text = "// a note\nexport function shipped() {}\nfunction local() {}\n";
        let document = analyze(text);
        let shipped = &document.symbols()[0];
        assert_eq!(spanned(text, shipped.range), "export function shipped() {}");
        assert_eq!(spanned(text, shipped.item_range), "function shipped() {}");
        let local = &document.symbols()[1];
        assert_eq!(local.range, local.item_range, "nothing carries `local`");
    }

    fn documentation_of<'document>(
        document: &'document SyntaxDocument,
        name: &str,
    ) -> Option<&'document str> {
        document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == name)
            .and_then(|symbol| symbol.documentation.first())
            .map(|documentation| documentation.text.as_str())
    }

    #[test]
    fn attached_jsdoc_becomes_documentation_with_its_markers_stripped() {
        let text = "/**\n * Loads one config.\n *\n * @param path where it lives\n */\n\
                    function load(path) {}\n";
        let document = analyze(text);
        assert_eq!(
            documentation_of(&document, "load"),
            Some("Loads one config.\n\n@param path where it lives")
        );
        let symbol = &document.symbols()[0];
        assert_eq!(
            symbol.documentation[0].format,
            rift_protocol::read::DocumentationFormat::Markdown
        );
        assert_eq!(symbol.documentation_ranges.len(), 1);
        assert_eq!(symbol.documentation_ranges[0].start, 0);
        assert_eq!(symbol.range.start, 0, "the span covers its JSDoc");
    }

    /// A `JSDoc` block written with CRLF line endings strips each line's
    /// carriage return with its marker.
    #[test]
    fn a_crlf_jsdoc_block_strips_each_line_ending() {
        let text = "/**\r\n * First line.\r\n * Second line.\r\n */\r\nfunction load() {}\r\n";
        let document = analyze(text);
        assert_eq!(
            documentation_of(&document, "load"),
            Some("First line.\nSecond line.")
        );
    }

    #[test]
    fn jsdoc_above_export_or_const_attaches_to_the_declaration() {
        let text = "/** Exported. */\nexport function shown() {}\n\
                    /** Held. */\nexport const held = () => 1;\n\
                    /** Local. */\nconst first = 1, second = 2;\n";
        let document = analyze(text);
        assert_eq!(documentation_of(&document, "shown"), Some("Exported."));
        assert_eq!(documentation_of(&document, "held"), Some("Held."));
        assert_eq!(documentation_of(&document, "first"), Some("Local."));
        assert_eq!(documentation_of(&document, "second"), None);
    }

    /// The span climbs with the walk: the `JSDoc` above `export` or `const`
    /// sits inside the declaration's `range`, so reading the declaration's
    /// source shows it. A second declarator starts at its own syntax element.
    #[test]
    fn jsdoc_above_export_or_const_sits_inside_the_range() {
        let text = "/** Exported. */\nexport function shown() {}\n\
                    /** Held. */\nexport const held = () => 1;\n\
                    /** Local. */\nconst first = 1, second = 2;\n";
        let document = analyze(text);
        let spans: Vec<(&str, &str)> = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), spanned(text, symbol.range)))
            .collect();
        assert_eq!(
            spans,
            [
                ("shown", "/** Exported. */\nexport function shown() {}"),
                ("held", "/** Held. */\nexport const held = () => 1"),
                ("first", "/** Local. */\nconst first = 1"),
                ("second", "second = 2"),
            ]
        );
    }

    #[test]
    fn several_jsdoc_blocks_join_and_a_plain_comment_or_blank_line_detaches() {
        let text = "/** One. */\n/** Two. */\nfunction both() {}\n\
                    /** Hidden. */\n// plain\nfunction plain() {}\n\
                    /** Far. */\n\nfunction far() {}\n\
                    /* Block. */\nfunction block() {}\n\
                    /**   */\nfunction blank() {}\n";
        let document = analyze(text);
        assert_eq!(documentation_of(&document, "both"), Some("One.\nTwo."));
        assert_eq!(documentation_of(&document, "plain"), None);
        assert_eq!(documentation_of(&document, "far"), None);
        assert_eq!(documentation_of(&document, "block"), None);
        let blank = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "blank")
            .expect("blank declares");
        assert!(
            blank.documentation.is_empty() && blank.documentation_ranges.is_empty(),
            "a JSDoc block holding nothing once stripped publishes nothing"
        );
        assert_eq!(
            spanned(text, blank.range),
            "/**   */\nfunction blank() {}",
            "the empty block still joins the span"
        );
    }

    /// A JSX component parses through this provider without errors.
    #[test]
    fn test_jsx_component_parses_without_errors() {
        let text = "export function Banner({ label }) {\n  return <section className=\"banner\">{label}</section>;\n}\n";
        let document = analyze(text);
        assert!(!document.has_errors());
        assert_eq!(document.symbols()[0].name, "Banner");
        assert!(document.symbols()[0].kinds.contains(&SymbolKind::Public));
        assert!(
            document
                .syntax_elements()
                .iter()
                .any(|syntax_element| syntax_element.kind == "jsx_element"),
            "the parsed tree must carry the JSX element"
        );
    }

    /// JavaScript allows redeclaring a function name; the walk emits both
    /// declarations rather than dropping either, under the one name and numbered
    /// apart in source order.
    #[test]
    fn test_redeclared_function_emits_both_declarations_numbered_apart() {
        let document = analyze("function dup() { return 1; }\nfunction dup() { return 2; }\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["dup", "dup"]);
        let ordinals = document
            .symbols()
            .iter()
            .map(|symbol| symbol.declaration_ordinal)
            .collect::<Vec<_>>();
        assert_eq!(ordinals, [0, 1]);
        assert_ne!(
            document.symbols()[0].range,
            document.symbols()[1].range,
            "the two declarations keep their own spans"
        );
    }

    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = analyze("function broken( {\n");
        assert!(document.has_errors());
        assert!(!document.syntax_elements().is_empty());
    }

    #[test]
    fn test_provider_enforces_source_syntax_element_and_depth_limits() {
        let source_error = JavaScriptSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "let x = 1;",
                },
                SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            )
            .expect_err("source bound");
        assert_eq!(
            source_error.slug(),
            rift_error::errors::syntax::source_too_large::SLUG
        );

        let syntax_element_error = JavaScriptSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "let x = 1;",
                },
                SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            )
            .expect_err("syntax element bound");
        assert_eq!(
            syntax_element_error.slug(),
            rift_error::errors::syntax::too_many_syntax_elements::SLUG
        );

        let depth_error = JavaScriptSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "function f() { if (x) { y(); } }",
                },
                SyntaxLimits::new(100, 50, 1).expect("positive limits"),
            )
            .expect_err("depth bound");
        assert_eq!(
            depth_error.slug(),
            rift_error::errors::syntax::too_deep::SLUG
        );
    }

    /// An empty source parses to a bare program syntax element under any positive
    /// bound.
    #[test]
    fn test_empty_source_parses_with_no_symbols() {
        let document = analyze("");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }

    /// Bytes past `PROVIDER_SYMBOL_ID_BYTES_MAX`, the name bound the
    /// Contribution contract enforces.
    const OVERSIZED_NAME_BYTES: usize = 9_000;
    const _: () = assert!(OVERSIZED_NAME_BYTES > PROVIDER_SYMBOL_ID_BYTES_MAX);

    /// A computed member key spells an expression of any length - the one
    /// name derivation a minified bundle pushes past the name bound. The
    /// method under it declares nothing; its class and the sibling method
    /// stay, and the document counts the one it left out.
    #[test]
    fn test_a_computed_key_past_the_name_bound_emits_no_symbol() {
        let key = "k".repeat(OVERSIZED_NAME_BYTES);
        let text = format!("class Bundle {{\n  [{key}]() {{}}\n  kept() {{}}\n}}\n");
        let document = analyze(&text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.container.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(names, [("Bundle", None), ("Bundle.kept", Some("Bundle"))]);
        assert_eq!(document.left_out_declaration_count(), 1);
    }

    /// A class named past the bound declares nothing, and neither does the
    /// method nested under it, whose qualified name carries the class name.
    #[test]
    fn test_a_class_past_the_name_bound_takes_its_methods_with_it() {
        let name = "c".repeat(OVERSIZED_NAME_BYTES);
        let text = format!("class {name} {{\n  run() {{}}\n}}\nfunction kept() {{}}\n");
        let document = analyze(&text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["kept"]);
        assert_eq!(document.left_out_declaration_count(), 2);
    }

    #[test]
    fn anonymous_export_assignment_keeps_property_and_original_function_ranges() {
        let source = "exports.useState = function (initialState) { return initialState; };\n";
        let document = analyze(source);
        let symbol = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "useState")
            .expect("original property binding");
        assert_eq!(symbol.kind, "function");
        assert_eq!(symbol.syntax_element_kind, Some("assignment_expression"));
        assert!(symbol.kinds.contains(&SymbolKind::Callable));
        assert!(symbol.kinds.contains(&SymbolKind::Public));
        assert_eq!(
            &source[usize::try_from(symbol.item_range.start).expect("item start")
                ..usize::try_from(symbol.item_range.end).expect("item end")],
            source.trim_end().trim_end_matches(';')
        );
        let name = symbol.name_range.expect("original property token");
        assert_eq!(
            &source[usize::try_from(name.start).expect("name start")
                ..usize::try_from(name.end).expect("name end")],
            "useState"
        );
        let body = symbol.body_range.expect("original anonymous body");
        assert_eq!(
            &source[usize::try_from(body.start).expect("body start")
                ..usize::try_from(body.end).expect("body end")],
            "{ return initialState; }"
        );
        assert_eq!(
            symbol.signatures[0].display,
            "exports.useState = function (initialState)"
        );
    }

    #[test]
    fn repeated_anonymous_export_assignments_keep_distinct_original_ranges() {
        let source = "exports.open = function () { return 1; };\nexports.open = function () { return 2; };\n";
        let document = analyze(source);
        let symbols = document
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == "open")
            .collect::<Vec<_>>();
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].qualified_name, symbols[1].qualified_name);
        assert_eq!(
            (
                symbols[0].declaration_ordinal,
                symbols[1].declaration_ordinal
            ),
            (0, 1)
        );
        assert!(symbols[0].range.end < symbols[1].range.start);
        for symbol in symbols {
            assert_eq!(symbol.syntax_element_kind, Some("assignment_expression"));
            assert!(symbol.kinds.contains(&SymbolKind::Public));
        }
        let changed = analyze(&source.replace("return 2", "return 3"));
        assert_eq!(changed.symbols().len(), document.symbols().len());
        assert_ne!(
            changed.facts().source_digest(),
            document.facts().source_digest()
        );
    }

    #[test]
    fn shadowed_and_dynamic_export_contexts_keep_raw_anonymous_binding() {
        for source in [
            "function outer(exports) { exports.open = function () {}; }",
            "(function (exports) { exports.open = function () {}; })(other);",
            "(function () { let exports = other; exports.open = function () {}; })();",
            "run(function () { exports.open = function () {}; });",
        ] {
            let document = analyze(source);
            let symbol = document
                .symbols()
                .iter()
                .find(|symbol| symbol.name == "open")
                .expect("physical anonymous property binding");
            assert!(!symbol.kinds.contains(&SymbolKind::Public), "{source}");
            assert!(
                document
                    .facts()
                    .export_bindings()
                    .expect("recorded export facts")
                    .is_empty()
            );
            let start = usize::try_from(symbol.item_range.start).expect("item start");
            let end = usize::try_from(symbol.item_range.end).expect("item end");
            assert_eq!(&source[start..end], "exports.open = function () {}");
        }
    }

    #[test]
    fn dynamic_property_and_non_export_assignments_remain_unassociated() {
        for source in [
            "exports[key] = function () {};",
            "other.open = function () {};",
            "exports.open = void 0;",
            "run(function () { exports.open = factory(); });",
        ] {
            let document = analyze(source);
            assert!(
                !document
                    .symbols()
                    .iter()
                    .any(|symbol| symbol.name == "open"),
                "{source}"
            );
        }
    }

    #[test]
    fn original_react_anonymous_property_assignments_keep_full_source_ranges() {
        for (original, original_start, module_statement) in [
            (
                include_str!("../tests/fixtures/react/react.development.js"),
                48_434_u64,
                false,
            ),
            (
                include_str!("../tests/fixtures/react/react.production.js"),
                17_608_u64,
                true,
            ),
        ] {
            let lf = original.replace("\r\n", "\n");
            let crlf = lf.replace('\n', "\r\n");
            for source in [original, lf.as_str(), crlf.as_str()] {
                let prefix = "exports.useState = function (initialState)";
                let starts = source.match_indices(prefix).collect::<Vec<_>>();
                assert_eq!(starts.len(), 1, "one original property assignment");
                let expected_start = starts[0].0;
                assert_eq!(
                    u64::try_from(source[..expected_start].replace("\r\n", "\n").len())
                        .expect("original LF offset"),
                    original_start,
                );
                let document = analyze(source);
                let symbol = document
                    .symbols()
                    .iter()
                    .find(|symbol| symbol.name == "useState")
                    .expect("actual original anonymous export");
                assert_eq!(
                    symbol.range.start,
                    u64::try_from(expected_start).expect("supplied source offset")
                );
                assert_eq!(symbol.syntax_element_kind, Some("assignment_expression"));
                assert_eq!(symbol.kinds.contains(&SymbolKind::Public), module_statement);
                let start = usize::try_from(symbol.item_range.start).expect("item start");
                let end = usize::try_from(symbol.item_range.end).expect("item end");
                assert!(source[start..end].starts_with(prefix));
                assert!(source[start..end].ends_with('}'));
                assert_eq!(symbol.signatures[0].display, prefix);
            }
        }
    }
}
