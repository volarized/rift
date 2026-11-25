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
    assert_eq!(module.syntax_element_kind, Some("module"));
    let declaration = document
        .syntax_elements()
        .iter()
        .find(|syntax_element| {
            syntax_element.kind == "module" && syntax_element.range == module.item_range
        })
        .expect("original module declaration syntax element");
    let statement = document
        .syntax_elements()
        .get(declaration.parent.expect("ambient parent"))
        .expect("original ambient statement");
    assert_eq!(statement.kind, "ambient_declaration");
    assert_eq!(token(types, Some(module.range)), Some(types.trim_end()));
    assert_eq!(token(types, Some(statement.range)), Some(types.trim_end()));
    assert_eq!(
        token(types, Some(module.item_range)),
        Some("module \"fs\" { export function readFile(path: string): void; }"),
    );
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
    assert!(document.symbols()[0].kinds.contains(&SymbolKind::Public));
    assert!(!document.symbols()[2].kinds.contains(&SymbolKind::Public));
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
    let text = "type Input = string;\nconst value = 1;\nexport { type Input as Options, value };\nexport type { Input as Config } from './types';\nexport * from './all';\nexport type * from './type-all';\nexport * as tools from './tools';\nexport interface Shape { size: number }\n";
    let path = rift_core::ProjectPath::new("index.ts").expect("fixture path");
    for dialect in [TypeScriptDialect::TypeScript, TypeScriptDialect::Tsx] {
        let provider = TypeScriptSyntaxProvider::new(dialect);
        let document = analyze(&provider, text);
        assert!(!document.facts().has_errors());
        let bindings = document
            .facts()
            .export_bindings()
            .expect("recorded exports");
        assert_eq!(bindings.len(), 7);
        assert_eq!(
            bindings
                .iter()
                .map(|binding| binding.type_only)
                .collect::<Vec<_>>(),
            [true, false, true, false, true, false, true]
        );
        assert_eq!(bindings[3].kind, SyntaxExportKind::All);
        assert_eq!(bindings[4].kind, SyntaxExportKind::All);
        assert_eq!(token(text, bindings[4].source), Some("'./type-all'"));
        assert_eq!(bindings[5].kind, SyntaxExportKind::Namespace);
        assert_eq!(token(text, bindings[5].exported), Some("tools"));
        assert_eq!(token(text, bindings[5].source), Some("'./tools'"));
        assert_eq!(token(text, bindings[6].exported), Some("Shape"));
        let malformed = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "export type * from;\n",
                },
                SyntaxLimits::default(),
            )
            .expect("malformed fixture source");
        assert!(malformed.has_errors());
        assert!(malformed.facts().export_bindings().is_none_or(|bindings| {
            bindings
                .iter()
                .all(|binding| binding.kind != SyntaxExportKind::All || !binding.type_only)
        }));
    }
}

#[test]
fn default_exports_keep_named_references_and_anonymous_forms() {
    for (text, local) in [
        ("function run() {}\nexport default run;", "run"),
        ("export default function run() {}", "run"),
        ("export default () => 1;", "() => 1"),
        ("export default function () {}", "function () {}"),
    ] {
        let document = analyze(&JavaScriptSyntaxProvider::default(), text);
        let bindings = document
            .facts()
            .export_bindings()
            .expect("recorded exports");
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].kind, SyntaxExportKind::Default);
        assert_eq!(token(text, bindings[0].local), Some(local));
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
        .expect_err("syntax element bound");
    assert_eq!(
        error.slug(),
        rift_error::errors::syntax::too_many_syntax_elements::SLUG
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
        assert!(format.kinds.contains(&SymbolKind::Public));
        assert!(format.kinds.contains(&SymbolKind::Callable));
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
        assert!(!hidden.kinds.contains(&SymbolKind::Public));
        assert!(bindings.iter().all(|binding| binding.container.is_none()));
        assert!(
            document
                .symbols()
                .iter()
                .filter(|symbol| { matches!(symbol.name.as_str(), "run" | "nested") })
                .all(|symbol| !symbol.kinds.contains(&SymbolKind::Public))
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
            assert!(symbol.kinds.contains(&SymbolKind::Value));
            assert_eq!(
                symbol.kinds.contains(&SymbolKind::Callable),
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

/// Every `CommonJS` form that names an export, beside the ones that name none.
#[test]
fn commonjs_assignments_bind_every_exported_name() {
    let source = "var proto = require('./application');
var taken = require('./taken');
exports = module.exports = create;
function create() {}
exports.application = proto;
exports.Route = taken.Route;
exports.json = { parse: function parse() {} };
exports.Client = class Client { connect() {} };
exports.open = exports.load = function load() {};
exports.placeholder = exports.later = void 0;
exports.taken = taken.value;
";
    let document = analyze(&JavaScriptSyntaxProvider::default(), source);
    assert_eq!(
        commonjs_bindings(source, &document),
        [
            (SyntaxExportKind::Default, None, Some("create"), None),
            named("application", "proto"),
            named("Route", "Route"),
            named("json", "json"),
            named("Client", "Client"),
            named("open", "load"),
            named("load", "load"),
            named("taken", "taken"),
        ]
    );
    let declared = document
        .symbols()
        .iter()
        .filter(|symbol| symbol.kinds.contains(&SymbolKind::Public))
        .map(|symbol| (symbol.qualified_name.as_str(), symbol.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        declared,
        [
            ("proto", "variable"),
            ("create", "function"),
            ("Route", "variable"),
            ("json", "variable"),
            ("json.parse", "property"),
            ("Client", "class"),
            ("load", "function"),
            ("exports.taken", "variable"),
        ]
    );
    assert!(
        document
            .symbols()
            .iter()
            .any(|symbol| symbol.qualified_name == "Client.connect" && symbol.kind == "method")
    );
}

#[test]
fn commonjs_assignments_without_a_written_name_bind_nothing() {
    let source = "exports['quoted'] = 1;\nexports[computed] = 2;\nexports.plain = 3;\n";
    let document = analyze(&JavaScriptSyntaxProvider::default(), source);
    assert_eq!(
        commonjs_bindings(source, &document),
        [named("plain", "plain")]
    );
}

type Bound<'source> = (
    SyntaxExportKind,
    Option<&'source str>,
    Option<&'source str>,
    Option<&'source str>,
);

fn named<'source>(exported: &'source str, local: &'source str) -> Bound<'source> {
    (SyntaxExportKind::Named, Some(exported), Some(local), None)
}

fn commonjs_bindings<'source>(
    source: &'source str,
    document: &SyntaxDocument,
) -> Vec<Bound<'source>> {
    document
        .facts()
        .export_bindings()
        .expect("recorded exports")
        .iter()
        .map(|binding| {
            (
                binding.kind,
                token(source, binding.exported),
                token(source, binding.local),
                token(source, binding.source),
            )
        })
        .collect()
}

#[test]
fn commonjs_aliases_of_the_module_value_export_their_members() {
    for declarator in [
        "var app = exports = module.exports = {};",
        "var app = module.exports = exports = {};",
        "const app = module.exports = Object.create(null);",
        "const app = module.exports = { ready: true };",
    ] {
        let source = format!(
            "{declarator}\napp.init = function init() {{}};\napp.use = helper;\napp.size = 1;\nfunction helper() {{}}\nfunction hidden() {{ app.late = helper; }}\n"
        );
        let document = analyze(&JavaScriptSyntaxProvider::default(), &source);
        let bindings = commonjs_bindings(&source, &document);
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.0 == SyntaxExportKind::Named && binding.1 != Some("ready"))
                .collect::<Vec<_>>(),
            [&named("init", "init"), &named("use", "helper")],
            "{declarator}"
        );
        assert!(
            bindings.contains(&(SyntaxExportKind::Default, None, Some("app"), None)),
            "{declarator}"
        );
        for (name, public) in [
            ("app", true),
            ("app.init", true),
            ("helper", true),
            ("hidden", false),
        ] {
            let symbol = document
                .symbols()
                .iter()
                .find(|symbol| symbol.qualified_name == name)
                .expect("declared name");
            assert_eq!(
                symbol.kinds.contains(&SymbolKind::Public),
                public,
                "{declarator} {name}"
            );
        }
        if declarator.contains("ready") {
            assert!(document.symbols().iter().any(|symbol| {
                symbol.qualified_name == "app.ready" && symbol.kinds.contains(&SymbolKind::Public)
            }));
        }
    }
    for source in [
        "const out = exports; out.open = function open() {};",
        "const out = module.exports; out.open = function open() {};",
        "module.exports = Base; const out = module.exports; out.open = function open() {};",
    ] {
        let document = analyze(&JavaScriptSyntaxProvider::default(), source);
        assert!(
            commonjs_bindings(source, &document).contains(&named("open", "open")),
            "{source}"
        );
    }
    for source in [
        "let app = module.exports = {}; app = other; app.init = function init() {};",
        "let app = module.exports = {}; app ||= other; app.init = function init() {};",
        "let app = module.exports = {}; app++; app.init = function init() {};",
        "let app = module.exports = {}; [app] = other; app.init = function init() {};",
        "let app = module.exports = {}; for (app of other) {} app.init = function init() {};",
        "var app = module.exports = {}; var app = other; app.init = function init() {};",
        "var app = module.exports = {}; function app() {} app.init = function init() {};",
        "var app = module.exports = {}; module.exports ||= other; app.init = function init() {};",
        "var app = module.exports = {}; module['exports'] = other; app.init = function init() {};",
        "var app = module.exports = {}; module.exports = other; app.init = function init() {};",
        "var app = exports = {}; app.init = function init() {};",
        "const out = exports; module.exports = other; out.init = function init() {};",
        "function wrap() { var app = exports = module.exports = {}; app.init = function init() {}; }",
        "var app = exports = module.exports = {}; exports = other; app.init = function init() {};",
        "var app = exports = module.exports = {}; if (ready) { app.init = function init() {}; }",
        "import { other } from './other.js'; var app = local = {}; app.init = function init() {};",
    ] {
        let document = analyze(&JavaScriptSyntaxProvider::default(), source);
        assert!(
            !commonjs_bindings(source, &document)
                .iter()
                .any(|binding| binding.1 == Some("init")),
            "{source}"
        );
        assert!(
            !document.symbols().iter().any(|symbol| {
                symbol.name == "init" && symbol.kinds.contains(&SymbolKind::Public)
            }),
            "{source}"
        );
    }
}

#[test]
fn commonjs_typescript_output_binds_defined_properties_and_copied_modules() {
    let source = "Object.defineProperty(exports, \"__esModule\", { value: true });
exports.answer = exports.open = void 0;
const util_1 = require('./util');
Object.defineProperty(exports, 'helper', { enumerable: true, get: function () { return util_1.helper; } });
Object.defineProperty(exports, 'plain', { value: open });
Object.defineProperty(exports, 'getter', { get() { return open; } });
Object.defineProperty(exports, 'arrow', { get: () => open });
Object.defineProperty(exports, name, { value: open });
Object.defineProperty(exports, 'spread', { ...other, value: open });
Object.defineProperty(other, 'foreign', { value: open });
__exportStar(require('./shapes'), exports);
tslib_1.__exportStar(require('./types'), exports);
__export(require('./older'));
function open() {}
exports.open = open;
";
    let document = analyze(&JavaScriptSyntaxProvider::default(), source);
    let all = |module| (SyntaxExportKind::All, None, None, Some(module));
    assert_eq!(
        commonjs_bindings(source, &document),
        [
            named("helper", "util_1.helper"),
            named("plain", "open"),
            named("getter", "open"),
            named("arrow", "open"),
            all("'./shapes'"),
            all("'./types'"),
            all("'./older'"),
            named("open", "open"),
        ]
    );
    let rebound = format!("{source}exports = other;\n");
    let document = analyze(&JavaScriptSyntaxProvider::default(), &rebound);
    assert!(commonjs_bindings(&rebound, &document).is_empty());
}

#[test]
fn commonjs_values_bind_a_required_property_and_each_conditional_module() {
    let bound = |source: &str| {
        let document = analyze(&JavaScriptSyntaxProvider::default(), source);
        commonjs_bindings(source, &document)
            .into_iter()
            .map(|(kind, _, local, module)| {
                (kind, local.map(str::to_owned), module.map(str::to_owned))
            })
            .collect::<Vec<_>>()
    };
    let all = |module: &str| (SyntaxExportKind::All, None, Some(module.to_owned()));
    for source in [
        "module.exports = require('./x').K;",
        "exports = module.exports = require('./x').K;",
    ] {
        assert_eq!(
            bound(source),
            [(
                SyntaxExportKind::Default,
                Some("K".to_owned()),
                Some("'./x'".to_owned())
            )],
            "{source}"
        );
    }
    assert_eq!(
        bound("module.exports = c ? require('./a') : (require('./b'));"),
        [all("'./a'"), all("'./b'")]
    );
    for source in [
        "module.exports = c ? require('./a') : other;",
        "module.exports = c ? require('./a').K : require('./b');",
        "module.exports = require('./x')[K];",
        "module.exports = require('./x').K();",
        "module.exports = require(name).K;",
        "exports.name = c ? require('./a') : require('./b');",
    ] {
        assert!(
            bound(source).iter().all(|(_, _, module)| module.is_none()),
            "{source}"
        );
    }
}

#[test]
fn commonjs_literals_bind_pairs_no_later_property_replaces() {
    let bound = |literal: &str| {
        let source = format!("module.exports = {literal};\nfunction check() {{}}\n");
        let document = analyze(&JavaScriptSyntaxProvider::default(), &source);
        commonjs_bindings(&source, &document)
            .into_iter()
            .map(|binding| {
                (
                    binding.1.unwrap_or_default().to_owned(),
                    binding.2.unwrap_or_default().to_owned(),
                )
            })
            .collect::<Vec<_>>()
    };
    let pair = |exported: &str, local: &str| (exported.to_owned(), local.to_owned());
    assert_eq!(
        bound("{ kDirect: Symbol('direct'), exist: check, check }"),
        [
            pair("kDirect", "kDirect"),
            pair("exist", "check"),
            pair("check", "check"),
        ]
    );
    assert_eq!(
        bound("{ ...base, first: check, [key]: other, last: check }"),
        [pair("last", "check")]
    );
    assert_eq!(
        bound("{ twice: check, 'twice': other, once: check, get once() {} }"),
        []
    );
}

#[test]
fn ecmascript_module_syntax_leaves_assignments_to_exports_unexported() {
    let source =
        "export function open() {}\nexports.run = function run() {};\nexports.limit = 1;\n";
    let document = analyze(&JavaScriptSyntaxProvider::default(), source);
    for symbol in document.symbols() {
        assert_eq!(
            symbol.kinds.contains(&SymbolKind::Public),
            symbol.name == "open",
            "{}",
            symbol.qualified_name
        );
    }
}

/// A generated module with many aliases and writes keeps one binding for each write.
#[test]
fn commonjs_generated_aliases_bind_each_write_once() {
    use std::fmt::Write as _;

    let mut source = String::new();
    for index in 0..400 {
        write!(
            source,
            "var app{index} = exports;\napp{index}.run{index} = function () {{}};\n"
        )
        .expect("generated source");
    }
    let document = analyze(&JavaScriptSyntaxProvider::default(), &source);
    let bindings = commonjs_bindings(&source, &document);
    assert_eq!(bindings.len(), 400);
    for index in 0..400 {
        let name = format!("run{index}");
        assert!(
            bindings.contains(&named(&name, &name)),
            "write through alias {index}"
        );
    }
}
