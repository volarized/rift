use super::*;
use crate::javascript::JavaScriptSyntaxProvider;
use crate::provider::SyntaxProvider;
use crate::typescript::{TypeScriptDialect, TypeScriptSyntaxProvider};

fn analyze(provider: &dyn SyntaxProvider, text: &str) -> SyntaxDocument {
    let path = rift_core::ProjectPath::new("index.ts").expect("fixture path");
    let document = provider
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("fixture source");
    assert!(!document.has_errors());
    document
}

fn token(text: &str, range: Option<ByteRange>) -> Option<&str> {
    range.map(|range| {
        let start = usize::try_from(range.start).expect("fixture start");
        let end = usize::try_from(range.end).expect("fixture end");
        &text[start..end]
    })
}

#[test]
fn ambient_modules_keep_literal_scope_and_commonjs_exports_keep_statement_ranges() {
    let types = "declare module \"fs\" { export function readFile(path: string): void; }\n";
    let document = analyze(
        &TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript),
        types,
    );
    let module = document
        .symbols()
        .iter()
        .find(|symbol| symbol.kind == "namespace")
        .expect("ambient module");
    assert_eq!(module.node_kind, Some("ambient_declaration"));
    assert_eq!(token(types, module.name_range), Some("\"fs\""));
    let binding = &document.facts().export_bindings().expect("exports")[0];
    assert_eq!(
        binding.container.as_deref(),
        Some(module.qualified_name.as_str())
    );
    let source = "function readFile() {}\nmodule.exports = fs = { readFile };\nfunction hidden() { module.exports = { hidden }; }\n";
    let document = analyze(&JavaScriptSyntaxProvider::default(), source);
    let bindings = document
        .facts()
        .export_bindings()
        .expect("CommonJS exports");
    assert_eq!(bindings.len(), 1);
    assert_eq!(token(source, bindings[0].local), Some("readFile"));
    assert_eq!(
        token(source, Some(bindings[0].range)),
        Some("module.exports = fs = { readFile };")
    );
}

#[test]
fn named_aliases_keep_defining_names_and_reexport_sources() {
    let text = "function format2() {}\nclass SemVer { format() {} }\nexport { format2 as format };\nexport { format as exported, default as run } from './other.js';\n";
    let document = analyze(&JavaScriptSyntaxProvider::default(), text);
    let bindings = document
        .facts()
        .export_bindings()
        .expect("recorded exports");
    let names = bindings
        .iter()
        .map(|binding| {
            (
                token(text, binding.local),
                token(text, binding.exported),
                token(text, binding.source),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            (Some("format2"), Some("format"), None),
            (Some("format"), Some("exported"), Some("'./other.js'")),
            (Some("default"), Some("run"), Some("'./other.js'")),
        ]
    );
    assert!(
        bindings
            .iter()
            .all(|binding| binding.kind == SyntaxExportKind::Named)
    );
    assert_eq!(
        document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>(),
        ["format2", "SemVer", "SemVer.format"]
    );
    assert!(document.symbols()[0].facets.contains(&SymbolFacet::Public));
    assert!(!document.symbols()[2].facets.contains(&SymbolFacet::Public));
}

#[test]
fn every_exported_variable_declarator_retains_its_binding() {
    let text = "export const first = 1, second = 2, third = 3;\nexport function run() {}\nexport class Router { run() {} }\n";
    let document = analyze(&JavaScriptSyntaxProvider::default(), text);
    let bindings = document
        .facts()
        .export_bindings()
        .expect("recorded exports");
    assert_eq!(
        bindings
            .iter()
            .map(|binding| token(text, binding.exported))
            .collect::<Vec<_>>(),
        [
            Some("first"),
            Some("second"),
            Some("third"),
            Some("run"),
            Some("Router")
        ]
    );
    assert_eq!(bindings[0].range, bindings[1].range);
    assert_eq!(bindings[1].range, bindings[2].range);
    assert!(
        bindings
            .iter()
            .all(|binding| binding.local == binding.exported && binding.source.is_none())
    );
}

#[test]
fn typescript_export_forms_keep_type_and_namespace_bindings() {
    let text = "type Input = string;\nconst value = 1;\nexport { type Input as Options, value };\nexport type { Input as Config } from './types';\nexport * from './all';\nexport * as tools from './tools';\nexport interface Shape { size: number }\n";
    for dialect in [TypeScriptDialect::TypeScript, TypeScriptDialect::Tsx] {
        let document = analyze(&TypeScriptSyntaxProvider::new(dialect), text);
        let bindings = document
            .facts()
            .export_bindings()
            .expect("recorded exports");
        assert_eq!(bindings.len(), 6);
        assert_eq!(
            bindings
                .iter()
                .map(|binding| binding.type_only)
                .collect::<Vec<_>>(),
            [true, false, true, false, false, true]
        );
        assert_eq!(bindings[3].kind, SyntaxExportKind::All);
        assert_eq!(bindings[4].kind, SyntaxExportKind::Namespace);
        assert_eq!(token(text, bindings[4].exported), Some("tools"));
        assert_eq!(token(text, bindings[4].source), Some("'./tools'"));
        assert_eq!(token(text, bindings[5].exported), Some("Shape"));
    }
}

#[test]
fn default_exports_keep_named_references_and_anonymous_forms() {
    for text in [
        "function run() {}\nexport default run;",
        "export default function run() {}",
        "export default () => 1;",
        "export default function () {}",
    ] {
        let document = analyze(&JavaScriptSyntaxProvider::default(), text);
        let bindings = document
            .facts()
            .export_bindings()
            .expect("recorded exports");
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].kind, SyntaxExportKind::Default);
        assert_eq!(
            token(text, bindings[0].local),
            text.contains("run").then_some("run")
        );
        assert!(bindings[0].source.is_none());
    }
}

#[test]
fn exports_inside_a_namespace_keep_their_container() {
    let text = "namespace Api { export const first = 1, second = 2; export { first as chosen }; }";
    let document = analyze(
        &TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript),
        text,
    );
    let bindings = document
        .facts()
        .export_bindings()
        .expect("recorded exports");
    assert_eq!(bindings.len(), 3);
    assert!(
        bindings
            .iter()
            .all(|binding| binding.container.as_deref() == Some("Api"))
    );
    assert_eq!(token(text, bindings[2].exported), Some("chosen"));
}

#[test]
fn empty_exports_are_recorded_and_export_walk_keeps_existing_bounds() {
    let document = analyze(&JavaScriptSyntaxProvider::default(), "const local = 1;");
    assert_eq!(document.facts().export_bindings(), Some([].as_slice()));
    let path = rift_core::ProjectPath::new("index.js").expect("fixture path");
    let error = JavaScriptSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: "export { first, second, third };",
            },
            SyntaxLimits::new(100, 2, 10).expect("positive limits"),
        )
        .expect_err("node bound");
    assert_eq!(
        error.slug(),
        rift_error::errors::syntax::too_many_nodes::SLUG
    );
}

#[test]
fn declared_exports_keep_bindings_public_facts_and_complete_statement_ranges() {
    let text = "export declare function format(value: string): string;\nexport declare const first: string, second: number;\nexport declare class Router { run(): void; }\ndeclare function hidden(): void;\ndeclare namespace Hidden { function nested(): void; }\n";
    for dialect in [TypeScriptDialect::TypeScript, TypeScriptDialect::Tsx] {
        let document = analyze(&TypeScriptSyntaxProvider::new(dialect), text);
        let bindings = document
            .facts()
            .export_bindings()
            .expect("recorded exports");
        assert_eq!(
            bindings
                .iter()
                .map(|binding| token(text, binding.exported))
                .collect::<Vec<_>>(),
            [
                Some("format"),
                Some("first"),
                Some("second"),
                Some("Router")
            ]
        );
        assert_eq!(bindings[1].range, bindings[2].range);
        assert_eq!(
            token(text, Some(bindings[0].range)),
            Some("export declare function format(value: string): string;")
        );
        let format = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "format")
            .expect("declared function");
        assert!(format.facets.contains(&SymbolFacet::Public));
        assert!(format.facets.contains(&SymbolFacet::Callable));
        assert_eq!(format.range, bindings[0].range);
        assert_eq!(
            token(text, Some(format.item_range)),
            Some("function format(value: string): string;")
        );
        assert!(!format.signatures.is_empty());
        let hidden = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "hidden")
            .expect("unexported declaration");
        assert!(!hidden.facets.contains(&SymbolFacet::Public));
        assert!(bindings.iter().all(|binding| binding.container.is_none()));
        assert!(
            document
                .symbols()
                .iter()
                .filter(|symbol| { matches!(symbol.name.as_str(), "run" | "nested") })
                .all(|symbol| !symbol.facets.contains(&SymbolFacet::Public))
        );
    }
}

#[test]
fn declared_function_types_keep_variable_kind_and_callable_fact() {
    let text = "export declare const parse: (text: string) => Node;\nexport declare const value: string;\nexport declare const object: { call: () => void };\n";
    for dialect in [TypeScriptDialect::TypeScript, TypeScriptDialect::Tsx] {
        let document = analyze(&TypeScriptSyntaxProvider::new(dialect), text);
        for symbol in document.symbols() {
            if !matches!(symbol.name.as_str(), "parse" | "value" | "object") {
                continue;
            }
            assert_eq!(symbol.kind, "variable");
            assert!(symbol.signatures.is_empty());
            assert!(symbol.facets.contains(&SymbolFacet::Value));
            assert_eq!(
                symbol.facets.contains(&SymbolFacet::Callable),
                symbol.name == "parse"
            );
        }
        assert!(
            document
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "parse")
        );
    }
}
