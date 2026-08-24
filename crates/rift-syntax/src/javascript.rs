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
use crate::provider::{
    SYNTAX_DEPTH_MAX_DEFAULT, SYNTAX_NODES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

/// Bounded Tree-sitter JavaScript fact provider.
#[derive(Debug, Clone)]
pub struct JavaScriptSyntaxProvider {
    language: Language,
    limits: SyntaxLimits,
}

impl JavaScriptSyntaxProvider {
    /// File extensions this provider parses, without their leading dot. The
    /// pinned grammar parses JSX, so `jsx` files need no dialect of their
    /// own.
    pub const SOURCE_EXTENSIONS: &'static [&'static str] = &["js", "jsx"];

    /// Default maximum bytes this provider accepts from one JavaScript
    /// source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider with explicit bounds.
    #[must_use]
    pub fn new(limits: SyntaxLimits) -> Self {
        Self {
            language: Language {
                name: "javascript".to_owned(),
                dialect: None,
            },
            limits,
        }
    }
}

/// The JavaScript provider's declared default bounds, proven positive at
/// compile time.
const JAVASCRIPT_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    JavaScriptSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl Default for JavaScriptSyntaxProvider {
    fn default() -> Self {
        Self::new(JAVASCRIPT_SYNTAX_LIMITS_DEFAULT)
    }
}

impl SyntaxProvider for JavaScriptSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn extensions(&self) -> &'static [&'static str] {
        Self::SOURCE_EXTENSIONS
    }

    fn source_bytes_max(&self) -> usize {
        self.limits.source_bytes_max()
    }

    fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError> {
        ecmascript::analyze(
            &self.language,
            &javascript_grammar(),
            javascript_kinds(),
            self.limits,
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
    use rift_core::ProjectPath;
    use rift_protocol::read::SymbolFacet;

    use super::*;
    use crate::failure::SyntaxViolation;

    fn path() -> ProjectPath {
        ProjectPath::new("src/app.js").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        JavaScriptSyntaxProvider::default()
            .analyze(SyntaxSource {
                path: &path(),
                text,
            })
            .expect("JavaScript fixture must parse")
    }

    #[test]
    fn test_provider_declares_language_extensions_and_byte_bound() {
        let provider = JavaScriptSyntaxProvider::default();
        assert_eq!(provider.language().name, "javascript");
        assert_eq!(provider.language().dialect, None);
        assert_eq!(provider.extensions(), ["js", "jsx"]);
        assert_eq!(
            provider.source_bytes_max(),
            JavaScriptSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
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
    /// declarations under one qualified name rather than renaming or
    /// dropping either.
    #[test]
    fn test_redeclared_function_emits_both_declarations_under_one_qualified_name() {
        let document = analyze("function dup() { return 1; }\nfunction dup() { return 2; }\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["dup", "dup"]);
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
        let source_error =
            JavaScriptSyntaxProvider::new(SyntaxLimits::new(3, 10, 10).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "let x = 1;",
                })
                .expect_err("source bound");
        assert_eq!(
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error =
            JavaScriptSyntaxProvider::new(SyntaxLimits::new(100, 1, 10).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "let x = 1;",
                })
                .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
        );

        let depth_error =
            JavaScriptSyntaxProvider::new(SyntaxLimits::new(100, 50, 1).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "function f() { if (x) { y(); } }",
                })
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
}
