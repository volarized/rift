//! TypeScript and TSX syntax facts from the pinned tree-sitter-typescript
//! grammars.
//!
//! The crate ships two grammars: plain TypeScript, where an angle bracket
//! opens a type construct, and TSX, where it opens a JSX element. One
//! provider type serves both as dialects of the `typescript` language;
//! declaration rules live in [`crate::ecmascript`], shared with the
//! JavaScript provider.

use std::sync::OnceLock;

use rift_protocol::read::{Language, NodeFacet};

use crate::document::SyntaxDocument;
use crate::ecmascript::{self, EcmaScriptKinds};
use crate::failure::SyntaxError;
use crate::provider::{
    SYNTAX_DEPTH_MAX_DEFAULT, SYNTAX_NODES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

/// Which pinned tree-sitter-typescript grammar a provider instance reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeScriptDialect {
    /// Plain TypeScript: `ts` files.
    TypeScript,
    /// TSX: `tsx` files, where JSX changes how angle brackets parse.
    Tsx,
}

impl TypeScriptDialect {
    /// The provider instance the registry ships for this dialect, under the
    /// declared default bounds.
    #[must_use]
    pub fn provider(self) -> TypeScriptSyntaxProvider {
        TypeScriptSyntaxProvider::new(self, TYPESCRIPT_SYNTAX_LIMITS_DEFAULT)
    }

    fn language(self) -> Language {
        Language {
            name: "typescript".to_owned(),
            dialect: match self {
                Self::TypeScript => None,
                Self::Tsx => Some("tsx".to_owned()),
            },
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    /// Returns this dialect's process-wide resolved kind table, computing it
    /// once. The two grammars number their kinds independently, so each
    /// dialect resolves its own table.
    fn kinds(self) -> &'static EcmaScriptKinds {
        static TYPESCRIPT_KINDS: OnceLock<EcmaScriptKinds> = OnceLock::new();
        static TSX_KINDS: OnceLock<EcmaScriptKinds> = OnceLock::new();
        let cache = match self {
            Self::TypeScript => &TYPESCRIPT_KINDS,
            Self::Tsx => &TSX_KINDS,
        };
        cache.get_or_init(|| EcmaScriptKinds::resolve_typescript(&self.grammar()))
    }
}

/// Bounded Tree-sitter TypeScript fact provider, one instance per dialect.
#[derive(Debug, Clone)]
pub struct TypeScriptSyntaxProvider {
    dialect: TypeScriptDialect,
    language: Language,
    limits: SyntaxLimits,
}

impl TypeScriptSyntaxProvider {
    /// Default maximum bytes this provider accepts from one TypeScript
    /// source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider for one dialect with explicit bounds.
    #[must_use]
    pub fn new(dialect: TypeScriptDialect, limits: SyntaxLimits) -> Self {
        Self {
            dialect,
            language: dialect.language(),
            limits,
        }
    }
}

/// The TypeScript providers' declared default bounds, proven positive at
/// compile time.
const TYPESCRIPT_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    TypeScriptSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl SyntaxProvider for TypeScriptSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn source_bytes_max(&self) -> usize {
        self.limits.source_bytes_max()
    }

    fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError> {
        ecmascript::analyze(
            &self.language,
            &self.dialect.grammar(),
            self.dialect.kinds(),
            self.limits,
            source,
        )
    }

    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        ecmascript::node_facets(kind)
    }
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_protocol::read::SymbolFacet;

    use super::*;
    use crate::failure::SyntaxViolation;

    fn path() -> ProjectPath {
        ProjectPath::new("src/app.ts").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        TypeScriptDialect::TypeScript
            .provider()
            .analyze(SyntaxSource {
                path: &path(),
                text,
            })
            .expect("TypeScript fixture must parse")
    }

    #[test]
    fn test_providers_declare_language_dialect_and_byte_bound() {
        let typescript = TypeScriptDialect::TypeScript.provider();
        assert_eq!(typescript.language().name, "typescript");
        assert_eq!(typescript.language().dialect, None);
        assert_eq!(
            typescript.source_bytes_max(),
            TypeScriptSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );

        let tsx = TypeScriptDialect::Tsx.provider();
        assert_eq!(tsx.language().name, "typescript");
        assert_eq!(tsx.language().dialect.as_deref(), Some("tsx"));
    }

    #[test]
    fn test_document_kind_words_cover_every_typescript_declaration_kind() {
        let text = "interface Route { path: string }\nenum Mode { Fast }\ntype Alias = Route;\nnamespace Registry {}\ndeclare function ambient(): void;\nfunction plain(): void {}\nclass Widget { open(): void {} }\nconst limit = 1;\n";
        let document = analyze(text);
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                ("Route", "interface"),
                ("Mode", "enum"),
                ("Alias", "type_alias"),
                ("Registry", "namespace"),
                ("ambient", "function"),
                ("plain", "function"),
                ("Widget", "class"),
                ("open", "method"),
                ("limit", "variable"),
            ]
        );
        assert!(!document.has_errors());
    }

    #[test]
    fn test_namespace_bodies_nest_declarations_under_the_namespace_name() {
        let text = "namespace Registry {\n  export function lookup(): void {}\n  export const limit = 1;\n}\n";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.container.as_deref(),
                    symbol.facets.contains(&SymbolFacet::Public),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("Registry", None, false),
                ("Registry.lookup", Some("Registry"), true),
                ("Registry.limit", Some("Registry"), true),
            ]
        );
    }

    /// The `visibility` field carries only an authored
    /// `accessibility_modifier`; a method without one stays `None`, and
    /// `export` never spells visibility.
    #[test]
    fn test_method_visibility_is_the_authored_accessibility_modifier() {
        let text = "export class Widget {\n  private hidden(): void {}\n  protected shared(): void {}\n  public open(): void {}\n  plain(): void {}\n}\n";
        let document = analyze(text);
        let spellings = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.visibility.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            spellings,
            [
                ("Widget", None),
                ("hidden", Some("private")),
                ("shared", Some("protected")),
                ("open", Some("public")),
                ("plain", None),
            ]
        );
    }

    #[test]
    fn test_document_facets_render_kind_categories_and_exported_public() {
        let text = "export interface Route { path: string }\nexport enum Mode { Fast }\nexport type Alias = Route;\nexport namespace Registry {}\ninterface Hidden {}\n";
        let document = analyze(text);
        let facets = document
            .symbols()
            .iter()
            .map(|symbol| symbol.facets.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            facets,
            [
                vec![SymbolFacet::Type, SymbolFacet::Public],
                vec![SymbolFacet::Type, SymbolFacet::Public],
                vec![SymbolFacet::Type, SymbolFacet::Alias, SymbolFacet::Public],
                vec![SymbolFacet::Namespace, SymbolFacet::Public],
                vec![SymbolFacet::Type],
            ]
        );
    }

    /// Body ranges span the interface, enum, namespace, and class bodies,
    /// the type alias value, and stay absent on bodyless signatures and
    /// ambient declarations.
    #[test]
    fn test_body_range_present_exactly_where_the_node_declares_one() {
        let text = "interface Route { path: string }\nenum Mode { Fast }\ntype Alias = Route;\nnamespace Registry { export const limit = 1; }\ndeclare function ambient(): void;\ndeclare const bare: number;\nclass Widget { open(): void {} }\n";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.body_range.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("Route", true),
                ("Mode", true),
                ("Alias", true),
                ("Registry", true),
                ("limit", true),
                ("ambient", false),
                ("bare", false),
                ("Widget", true),
                ("open", true),
            ]
        );
        let alias = &document.symbols()[2];
        let value = alias.body_range.expect("the alias holds a value");
        let start = usize::try_from(value.start).expect("fixture span fits usize");
        let end = usize::try_from(value.end).expect("fixture span fits usize");
        assert_eq!(&text[start..end], "Route");
    }

    /// The tsx dialect extracts the same declaration vocabulary as plain
    /// typescript, JSX values included.
    #[test]
    fn test_tsx_documents_extract_the_typescript_declaration_kinds() {
        let text = "export interface BannerProps { label: string }\nconst render = <T,>(value: T) => <div>{value}</div>;\nexport class Banner {\n  private draw(): void {}\n}\n";
        let tsx_path = ProjectPath::new("src/Banner.tsx").expect("valid fixture path");
        let document = TypeScriptDialect::Tsx
            .provider()
            .analyze(SyntaxSource {
                path: &tsx_path,
                text,
            })
            .expect("TSX fixture must parse");
        assert!(!document.has_errors());
        let facts = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.kind,
                    symbol.facets.contains(&SymbolFacet::Public),
                    symbol.visibility.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            facts,
            [
                ("BannerProps", "interface", true, None),
                ("render", "variable", false, None),
                ("Banner", "class", true, None),
                ("Banner.draw", "method", false, Some("private")),
            ]
        );
    }

    /// The dialect split is real: TSX bytes parse clean through the `tsx`
    /// dialect and break through plain `typescript`, where an angle bracket
    /// opens a type construct.
    #[test]
    fn test_tsx_bytes_split_the_dialects() {
        let text = "const render = <T,>(value: T) => <div>{value}</div>;\nexport function App(): unknown {\n  return <main>{render(\"beacon\")}</main>;\n}\n";
        let tsx_path = ProjectPath::new("src/App.tsx").expect("valid fixture path");
        let through_tsx = TypeScriptDialect::Tsx
            .provider()
            .analyze(SyntaxSource {
                path: &tsx_path,
                text,
            })
            .expect("TSX fixture must parse through the tsx dialect");
        assert!(!through_tsx.has_errors());
        assert_eq!(through_tsx.language().dialect.as_deref(), Some("tsx"));
        assert!(
            through_tsx
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "App"),
            "the component declaration must extract through the tsx dialect"
        );

        let through_typescript = TypeScriptDialect::TypeScript
            .provider()
            .analyze(SyntaxSource {
                path: &tsx_path,
                text,
            })
            .expect("the parse itself completes; the tree carries the errors");
        assert!(
            through_typescript.has_errors(),
            "the same bytes must break through plain typescript"
        );
    }

    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = analyze("interface Broken {\n");
        assert!(document.has_errors());
        assert!(!document.nodes().is_empty());
    }

    #[test]
    fn test_provider_enforces_source_node_and_depth_limits() {
        let bounded = |limits: SyntaxLimits, text: &str| {
            TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript, limits).analyze(
                SyntaxSource {
                    path: &path(),
                    text,
                },
            )
        };
        let source_error = bounded(
            SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            "let x = 1;",
        )
        .expect_err("source bound");
        assert_eq!(
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error = bounded(
            SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            "let x = 1;",
        )
        .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
        );

        let depth_error = bounded(
            SyntaxLimits::new(100, 50, 1).expect("positive limits"),
            "function f() { if (x) { y(); } }",
        )
        .expect_err("depth bound");
        assert_eq!(depth_error.fault().violation(), SyntaxViolation::TooDeep);
    }

    #[test]
    fn test_empty_source_parses_with_no_symbols() {
        let document = analyze("");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }
}
