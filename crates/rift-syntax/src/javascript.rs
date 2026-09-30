//! JavaScript syntax facts from the pinned tree-sitter-javascript grammar.
//!
//! The grammar parses JSX, so this provider claims both `js` and `jsx`.
//! Declaration rules live in [`crate::ecmascript`], shared with the
//! TypeScript providers.

use std::sync::OnceLock;

use rift_protocol::read::{Language, NodeFacet};

use crate::document::SyntaxDocument;
use crate::ecmascript::{self, EcmaScriptKinds};
use crate::failure::SyntaxError;
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

impl SyntaxProvider for JavaScriptSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, SyntaxError> {
        ecmascript::analyze(
            &self.language,
            &javascript_grammar(),
            javascript_kinds(),
            limits,
            source,
        )
    }

    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        ecmascript::node_facets(kind)
    }
}

fn javascript_grammar() -> tree_sitter::Language {
    tree_sitter_javascript::LANGUAGE.into()
}

/// Returns the process-wide resolved JavaScript kind table, computing it
/// once.
fn javascript_kinds() -> &'static EcmaScriptKinds {
    static KINDS: OnceLock<EcmaScriptKinds> = OnceLock::new();
    KINDS.get_or_init(|| EcmaScriptKinds::resolve_javascript(&javascript_grammar()))
}

#[cfg(test)]
mod tests {
    use rift_core::{PROVIDER_SYMBOL_ID_BYTES_MAX, ProjectPath};
    use rift_protocol::read::SymbolFacet;

    use super::*;
    use crate::failure::SyntaxViolation;

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
                ("Router.#secret", Some("Router")),
                ("Router.route", Some("Router")),
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

    /// A destructuring declarator declares its names through a pattern, not
    /// a single name, and emits no symbol.
    #[test]
    fn test_destructuring_declarator_emits_no_symbol() {
        let document = analyze("const { first, second } = pair;\nconst kept = 1;\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["kept"]);
    }

    #[test]
    fn test_exported_declarations_carry_the_public_facet_and_no_visibility() {
        let text = "export function shipped() {}\nexport const limit = 1;\nexport default class Router {}\nfunction hidden() {}\n";
        let document = analyze(text);
        let facts = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.facets.contains(&SymbolFacet::Public),
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
    /// `Public` facet.
    fn public_facts(document: &SyntaxDocument) -> Vec<(&str, bool)> {
        document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.facets.contains(&SymbolFacet::Public),
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
                ("start", true),
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
                ("open~1", true),
                ("outer", false),
                ("open~2", false),
                ("limit~1", false),
                ("Router", false),
                ("Router.open", false),
                ("limit~2", true),
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
                ("stop", true),
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
        assert_eq!(public_facts(&document), [("helper", true)]);
    }

    /// An assignment marks nothing when its value is a function or class
    /// expression, a member access, or an object behind a property export,
    /// when its target is no export, or when a block holds it.
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
                ("load~1", false),
                ("load~2", false),
            ]
        );
    }

    #[test]
    fn test_document_facets_render_kind_categories() {
        let document = analyze(
            "export function shipped() {}\nclass Router { route() {} }\nconst limit = 1;\n",
        );
        let facets = document
            .symbols()
            .iter()
            .map(|symbol| symbol.facets.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            facets,
            [
                vec![
                    SymbolFacet::Value,
                    SymbolFacet::Callable,
                    SymbolFacet::Public
                ],
                vec![SymbolFacet::Type],
                vec![SymbolFacet::Value, SymbolFacet::Callable],
                vec![SymbolFacet::Value],
            ]
        );
    }

    /// Body ranges span the statement block, class body, or declarator
    /// value, and stay absent where the node omits the field.
    #[test]
    fn test_body_range_present_exactly_where_the_node_declares_one() {
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

    /// The span of an ECMAScript declaration is its own node: nothing
    /// attaches in front, so `range` equals `item_range`.
    #[test]
    fn test_declaration_range_equals_item_range() {
        let document = analyze("// a note\nexport function shipped() {}\n");
        let symbol = &document.symbols()[0];
        assert_eq!(symbol.range, symbol.item_range);
    }

    /// A JSX component parses through this provider without errors.
    #[test]
    fn test_jsx_component_parses_without_errors() {
        let text = "export function Banner({ label }) {\n  return <section className=\"banner\">{label}</section>;\n}\n";
        let document = analyze(text);
        assert!(!document.has_errors());
        assert_eq!(document.symbols()[0].name, "Banner");
        assert!(document.symbols()[0].facets.contains(&SymbolFacet::Public));
        assert!(
            document
                .nodes()
                .iter()
                .any(|node| node.kind == "jsx_element"),
            "the parsed tree must carry the JSX element"
        );
    }

    /// JavaScript allows redeclaring a function name; the walk emits both
    /// declarations rather than dropping either, and each takes a `~N`
    /// suffix so the two address distinct identities.
    #[test]
    fn test_redeclared_function_emits_both_declarations_under_distinct_names() {
        let document = analyze("function dup() { return 1; }\nfunction dup() { return 2; }\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["dup~1", "dup~2"]);
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
        assert!(!document.nodes().is_empty());
    }

    #[test]
    fn test_provider_enforces_source_node_and_depth_limits() {
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
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error = JavaScriptSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "let x = 1;",
                },
                SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            )
            .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
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
        assert_eq!(depth_error.fault().violation(), SyntaxViolation::TooDeep);
    }

    /// An empty source parses to a bare program node under any positive
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
}
