//! Stored JavaScript and TypeScript edges from captured source.

use rift_core::{ProjectPath, SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, WorkspaceIndex) {
    workspace_with_manifest(
        files,
        r#"{"name":"fixture","version":"1.0.0","type":"commonjs"}"#,
    )
}

fn workspace_with_manifest(
    files: &[(&str, &str)],
    manifest: &str,
) -> (tempfile::TempDir, WorkspaceIndex) {
    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(directory.path().join("package.json"), manifest).expect("manifest");
    for (path, source) in files {
        std::fs::write(directory.path().join(path), source).expect("source");
    }
    let index = WorkspaceIndex::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    (directory, index)
}

#[test]
fn test_typescript_merged_alias_type_uses_preserve_the_value_namespace() {
    let source = "export namespace Result { export function make() { return 1; } }
export type Result<T> = { value: T };
export interface Work {
    andThen<R extends Result<number>>(run: () => R): Result<R>;
}
export type Runtime = typeof Result;
export type Shadow<Result> = Result;
export function create() { return Result.make(); }
";
    let (_directory, index) = workspace_with_manifest(
        &[("main.ts", source)],
        r#"{"name":"fixture","version":"1.0.0","type":"module"}"#,
    );
    let identity = "rift://symbol/local/typescript/.%2Fmain.ts/Result";
    let mut actual = index
        .edges()
        .edges()
        .filter(|edge| edge.kind() == EdgeKind::HasType && edge.to().as_str() == identity)
        .map(|edge| {
            let binding = edge.occurrence();
            assert_eq!(binding.unit().key().as_str(), "main.ts");
            let range = binding.range();
            let start = usize::try_from(range.start()).expect("type start");
            let end = usize::try_from(range.end()).expect("type end");
            assert_eq!(&source[start..end], "Result");
            start
        })
        .collect::<Vec<_>>();
    actual.sort_unstable();
    assert_eq!(
        actual,
        [
            source.find("Result<number>").expect("constraint"),
            source.find("Result<R>").expect("return type"),
        ]
    );
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/create",
        "main.ts/Result/make"
    ));
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_typescript_export_overloads_store_one_identity_and_call() {
    let (_directory, index) = workspace_with_manifest(
        &[
            (
                "main.ts",
                "import { run } from './jobs.js'; export function root() { run(1); }",
            ),
            (
                "jobs.ts",
                "export function run(value: string): void; export function run(value: number): void; export function run(value: unknown) {}",
            ),
        ],
        r#"{"name":"fixture","version":"1.0.0","type":"module"}"#,
    );
    let calls = index
        .edges()
        .edges()
        .filter(|edge| edge.kind() == EdgeKind::Calls)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].from().as_str().ends_with(".%2Fmain.ts/root"));
    assert!(calls[0].to().as_str().ends_with(".%2Fjobs.ts/run"));
    let view = index.symbol_view().expect("view");
    let record = view
        .normalized_graph()
        .records()
        .iter()
        .find(|record| {
            record
                .identity()
                .is_some_and(|identity| identity.as_str().ends_with(".%2Fjobs.ts/run"))
        })
        .expect("overload identity");
    assert_eq!(record.contributions().len(), 3);
}

#[test]
fn test_commonjs_local_object_members_store_distinct_lexical_identities() {
    let source = "var res = {}; res.clear = function clear() { const opts = { expires: 1 }; return opts.expires; }; res.set = function() { const opts = {}; opts.expires = 2; return opts; }; module.exports = res;";
    for path in ["main.cjs", "main.cts"] {
        let (_directory, index) = workspace(&[(path, source)]);
        let reads = index
            .edges()
            .edges()
            .filter(|edge| {
                edge.kind() == EdgeKind::Reads && edge.to().as_str().ends_with("/expires")
            })
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 1);
        assert!(reads[0].to().as_str().ends_with("/res/clear/opts/expires"));
        assert!(!index.edges().edges().any(
            |edge| edge.kind() == EdgeKind::Writes && edge.to().as_str().ends_with("/expires")
        ));
        for owner in ["clear", "set"] {
            assert!(
                index
                    .edges()
                    .edges()
                    .any(|edge| edge.kind() == EdgeKind::Contains
                        && edge.from().as_str().ends_with(&format!("/res/{owner}"))
                        && edge.to().as_str().ends_with(&format!("/res/{owner}/opts")))
            );
        }
        let language =
            rift_protocol::read::Language::from_identity_segment(if path.ends_with("cts") {
                "typescript"
            } else {
                "javascript"
            })
            .expect("language");
        assert!(
            index
                .identity_coverage_complete(&language)
                .expect("identity coverage")
        );
    }
}

#[test]
fn test_commonjs_callable_assignment_chain_keeps_the_inner_body_and_outer_alias() {
    let source = "var req = Object.create(null);
module.exports = req;
function value() { return 1; }
req.get =
req.header = function header() { return value(); };
function read() { return req.get(); }
function shadow() { const req = {}; req.get = 0; }

const different = {};
const other = {};
different.alias = other.inner = function inner() { return value(); };
function parameter(different) {
    different.alias = other.nested = function nested() { return value(); };
}
const changed = {};
changed.alias = changed.inner = function inner() { return value(); };
changed.alias = 0;
const replaced = {};
replaced.alias = replaced.inner = function inner() { return value(); };
replaced.inner = 0;
const computed = {};
const key = ['alias'].join('');
computed[key] = computed.inner = function inner() { return value(); };
const continued = {};
continued.alias = (continued.inner = function inner() { return value(); }, 0);
const returned = {};
returned.alias = returned.inner = value();
";
    for path in ["main.cjs", "main.cts"] {
        let (_directory, index) = workspace(&[(path, source)]);
        let from = format!("{path}/req/get");
        let to = format!("{path}/req/header");
        let aliases = index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Aliases)
            .collect::<Vec<_>>();
        assert_eq!(aliases.len(), 1, "{path} aliases");
        let edge = aliases[0];
        assert!(edge.from().as_str().ends_with(&from));
        assert!(edge.to().as_str().ends_with(&to));
        let range = edge.occurrence().range();
        let start = usize::try_from(range.start()).expect("start");
        let end = usize::try_from(range.end()).expect("end");
        assert_eq!(&source[start..end], "get");
        assert!(has_edge(
            &index,
            EdgeKind::Calls,
            &to,
            &format!("{path}/value")
        ));
        assert!(has_edge(
            &index,
            EdgeKind::Calls,
            &format!("{path}/read"),
            &from
        ));
        assert!(!has_edge(
            &index,
            EdgeKind::Calls,
            &from,
            &format!("{path}/value")
        ));
    }
}

#[test]
fn test_commonjs_prototype_methods_keep_hoisted_constructor_identities() {
    for path in ["main.cjs", "main.cts"] {
        let (_directory, index) = workspace(&[(
            path,
            "View.prototype.render = function() { leaf(); }; function View() {} function leaf() {}",
        )]);
        assert!(
            index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Calls
                    && edge.from().as_str().ends_with("/View/render")
                    && edge.to().as_str().ends_with("/leaf"))
        );
    }
}

#[test]
fn test_commonjs_prototype_methods_store_distinct_constructor_members() {
    let source = "function leaf() {} function View() {} View.prototype.render = function render() { leaf(); }; function Other() {} Other.prototype.lookup = function() { leaf(); };";
    for path in ["main.cjs", "main.cts"] {
        let (_directory, index) = workspace(&[(path, source)]);
        let members = index
            .edges()
            .edges()
            .filter(|edge| {
                edge.kind() == EdgeKind::Contains
                    && (edge.to().as_str().ends_with("/render")
                        || edge.to().as_str().ends_with("/lookup"))
            })
            .collect::<Vec<_>>();
        assert_eq!(members.len(), 2, "{path}: {members:?}");
        assert!(
            members
                .iter()
                .any(|edge| edge.from().as_str().ends_with("/View")
                    && edge.to().as_str().ends_with("/View/render"))
        );
        assert!(
            members
                .iter()
                .any(|edge| edge.from().as_str().ends_with("/Other")
                    && edge.to().as_str().ends_with("/Other/lookup"))
        );
        let calls = index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Calls)
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|edge| edge.to().as_str().ends_with("/leaf")
                    && (edge.from().as_str().ends_with("/View/render")
                        || edge.from().as_str().ends_with("/Other/lookup")))
        );
        let language =
            rift_protocol::read::Language::from_identity_segment(if path.ends_with("cts") {
                "typescript"
            } else {
                "javascript"
            })
            .expect("language");
        assert!(
            index
                .identity_coverage_complete(&language)
                .expect("identity coverage")
        );
    }
}

#[test]
fn test_commonjs_module_require_imports_store_known_file_targets() {
    for (path, other) in [("main.js", "other.js"), ("main.cts", "other.cts")] {
        for declaration in ["var", "let", "const"] {
            let source = format!(
                "{declaration} other = require('./other'); function use() {{ return other; }} module.exports = other;"
            );
            let (_directory, index) = workspace_with_manifest(
                &[
                    (path, &source),
                    (other, "var app = { value: 1 }; module.exports = app;"),
                ],
                r#"{"name":"fixture","version":"1.0.0"}"#,
            );
            let imports = index
                .edges()
                .edges()
                .filter(|edge| edge.kind() == EdgeKind::Imports)
                .collect::<Vec<_>>();
            assert_eq!(imports.len(), 1, "{path} {declaration}: {imports:?}");
            assert!(imports[0].from().as_str().contains("main."));
            assert!(imports[0].to().as_str().contains("other."));
            assert!(!imports[0].to().as_str().ends_with("/app"));
            assert!(index.edges().edges().any(
                |edge| edge.kind() == EdgeKind::Reads && edge.to().as_str().ends_with("/other")
            ));
            let language =
                rift_protocol::read::Language::from_identity_segment(if path.ends_with("cts") {
                    "typescript"
                } else {
                    "javascript"
                })
                .expect("language");
            assert!(
                index
                    .identity_coverage_complete(&language)
                    .expect("identity coverage")
            );
        }
    }
}

#[test]
fn test_ecmascript_parameter_bodies_store_unshadowed_calls() {
    // #792: parameters hide their own names while unrelated calls remain readable.
    let source = "function leaf() {}\nfunction run(value) { leaf(); }\nfunction shadow(leaf) { leaf(); }\nfunction pattern({ leaf: renamed }, [other], ...rest) { leaf(); renamed(); other(); rest(); }";
    for path in ["main.js", "main.ts"] {
        let (_directory, index) = workspace(&[(path, source)]);
        let calls = index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Calls)
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "{path}: {calls:?}");
        let owners = calls
            .iter()
            .map(|call| {
                call.from()
                    .as_str()
                    .rsplit('/')
                    .next()
                    .expect("caller name")
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(owners, std::collections::BTreeSet::from(["pattern", "run"]));
        for call in calls {
            assert!(call.to().as_str().ends_with("/leaf"));
        }
        assert!(index.edges().is_complete());
        assert_eq!(index.edges().dropped_edges(), 0);
    }
}

#[test]
fn test_commonjs_function_assignments_store_identities_and_calls() {
    // #799: each exported function keeps its body and local function-expression binding.
    let source = "function leaf() {}\nexports.run = function(value) { leaf(); };\nexports.other = function inner() { inner(); leaf(); };\nmodule.exports.third = function named() { leaf(); };\nfunction hidden(exports) { exports.run(); }\ninner();";
    for path in ["main.cjs", "main.cts"] {
        let (_directory, index) = workspace(&[(path, source)]);
        let calls = index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Calls)
            .map(|edge| {
                (
                    edge.from().as_str().rsplit('/').next().expect("caller"),
                    edge.to().as_str().rsplit('/').next().expect("callee"),
                )
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            calls,
            std::collections::BTreeSet::from([
                ("run", "leaf"),
                ("other", "other"),
                ("other", "leaf"),
                ("third", "leaf")
            ])
        );
        let writes = index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Writes)
            .collect::<Vec<_>>();
        assert_eq!(writes.len(), 3);
        for write in writes {
            assert_eq!(
                write.from().as_str(),
                write
                    .to()
                    .as_str()
                    .rsplit_once('/')
                    .expect("export parent")
                    .0
            );
        }
        assert!(
            index
                .identity_coverage_complete(
                    &rift_protocol::read::Language::from_identity_segment(
                        if path.ends_with("cts") {
                            "typescript"
                        } else {
                            "javascript"
                        }
                    )
                    .expect("language")
                )
                .expect("identity coverage")
        );
    }
}

#[test]
fn test_commonjs_function_assignments_store_anonymous_module_callers() {
    for path in ["main.cjs", "main.cts"] {
        let (_directory, index) = workspace(&[(
            path,
            "function leaf() {}\nmodule.exports = function(value) { return leaf(); };",
        )]);
        let calls = index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Calls)
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1, "{path}: {calls:?}");
        assert!(calls[0].from().as_str().ends_with("/default"));
        assert!(calls[0].to().as_str().ends_with("/leaf"));
    }
}

#[test]
fn test_commonjs_function_assignments_refuse_a_colliding_lexical_identity() {
    let source = "function run() {} exports.run = function run() { run(); }; run();";
    for path in ["main.cjs", "main.cts"] {
        let (_directory, index) = workspace(&[(path, source)]);
        assert!(
            !index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Calls)
        );
        let language =
            rift_protocol::read::Language::from_identity_segment(if path.ends_with("cts") {
                "typescript"
            } else {
                "javascript"
            })
            .expect("language");
        assert!(
            !index
                .identity_coverage_complete(&language)
                .expect("identity coverage")
        );
    }
}

#[test]
fn test_commonjs_function_assignments_refuse_colliding_default_slots() {
    for path in ["main.cjs", "main.cts"] {
        let source = "module.exports = function original() {}; module.exports.default = function member() {};";
        let (_directory, index) = workspace(&[(path, source)]);
        let language =
            rift_protocol::read::Language::from_identity_segment(if path.ends_with("cts") {
                "typescript"
            } else {
                "javascript"
            })
            .expect("language");
        assert!(
            !index
                .identity_coverage_complete(&language)
                .expect("identity coverage")
        );
    }
}

fn has_edge(index: &WorkspaceIndex, kind: EdgeKind, from: &str, to: &str) -> bool {
    index.edges().edges().any(|edge| {
        edge.kind() == kind
            && edge.from().as_str().ends_with(from)
            && edge.to().as_str().ends_with(to)
    })
}

fn module_manifest() -> &'static str {
    r#"{"name":"fixture","version":"1.0.0","type":"module"}"#
}

#[test]
fn test_typescript_contextual_global_names_store_symbols_and_method_edges() {
    let source = "export interface Arguments { value: number }\nexport class Instance {\n    boolean(keys: string | string[]): Instance { return this }\n    global(globals: string | string[], global?: boolean): Instance { return this.boolean(globals) }\n    check(f: (argv: Arguments) => number, global = true): Instance { return this.boolean(global ? 'x' : 'y') }\n    middleware(callback: () => void): Instance { return this.global('y') }\n}\n";
    for (path, language) in [("main.ts", "typescript"), ("main.tsx", "typescript:tsx")] {
        let (_directory, index) = workspace_with_manifest(&[(path, source)], module_manifest());
        let language = rift_protocol::read::Language::from_identity_segment(language)
            .expect("fixture language");
        assert!(
            index
                .identity_coverage_complete(&language)
                .expect("identity coverage"),
            "{path}"
        );
        assert!(index.edges().is_complete());
        for (kind, from, to) in [
            (EdgeKind::Contains, "/Instance", "/Instance/global"),
            (EdgeKind::Contains, "/Instance", "/Instance/check"),
            (EdgeKind::Calls, "/Instance/global", "/Instance/boolean"),
            (EdgeKind::Calls, "/Instance/check", "/Instance/boolean"),
            (EdgeKind::Calls, "/Instance/middleware", "/Instance/global"),
            (EdgeKind::HasType, "/Instance/check", "/Arguments"),
        ] {
            assert!(
                has_edge(&index, kind, from, to),
                "{path}: {kind:?} {from} to {to}"
            );
        }
    }
}

#[test]
fn test_typescript_newline_separated_generic_signatures_store_identities() {
    let source = "export type Handler<T> = {\n    (): T\n    <U>(value: U): U\n} & T\n\nexport type Container<T> = {\n    listen: {\n        (value: T): T\n        <U>(value: U): U\n    }\n}\n";
    for (path, language) in [("main.ts", "typescript"), ("main.tsx", "typescript:tsx")] {
        let (_directory, index) = workspace_with_manifest(&[(path, source)], module_manifest());
        let language = rift_protocol::read::Language::from_identity_segment(language)
            .expect("fixture language");
        assert!(
            index
                .identity_coverage_complete(&language)
                .expect("identity coverage"),
            "{path}"
        );
        let view = index.symbol_view().expect("symbol identities");
        for name in ["/Handler", "/Container", "/Container/listen"] {
            assert!(
                view.normalized_graph().records().iter().any(|record| {
                    record
                        .identity()
                        .is_some_and(|id| id.as_str().ends_with(name))
                }),
                "{path}: {name}"
            );
        }
        assert!(has_edge(
            &index,
            EdgeKind::Contains,
            "/Container",
            "/Container/listen"
        ));
        assert!(index.edges().is_complete());
    }
}

#[test]
fn test_typescript_variance_annotations_keep_identity_heritage_and_members() {
    let source = "export interface Base<T> { base: T }
export interface Mid<out T = unknown, in U = string> extends Base<T> { value: T; run(input: U): T }
export interface Wide<out Shape extends Base<string> = Base<string>> extends Mid<Shape> {}
export interface Leaf extends Mid<string> { leaf: number }
export class Box<in out T> extends Other<T> implements Mid<T> { value!: T; run(input: string): T { return this.value } }
export class Other<T> { other!: T }
export type Alias<out T> = Mid<T>;
export function use(item: Mid<string>, wide: Wide, alias: Alias<number>) { return item.value }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    let language =
        rift_protocol::read::Language::from_identity_segment("typescript").expect("language");
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("identity coverage")
    );
    for (from, to) in [
        ("/Mid", "/Base"),
        ("/Wide", "/Mid"),
        ("/Leaf", "/Mid"),
        ("/Box", "/Other"),
    ] {
        assert!(
            has_edge(&index, EdgeKind::Extends, from, to),
            "{from} extends {to}"
        );
    }
    assert!(has_edge(&index, EdgeKind::Implements, "/Box", "/Mid"));
    for member in ["/Mid/value", "/Mid/run"] {
        assert!(
            has_edge(&index, EdgeKind::Contains, "/Mid", member),
            "{member}"
        );
    }
    assert!(has_edge(&index, EdgeKind::Contains, "/Box", "/Box/value"));
    // Written constraint and default types outside the error stay type uses.
    assert!(has_edge(&index, EdgeKind::HasType, "/Wide", "/Base"));
    for target in ["/Mid", "/Wide", "/Alias"] {
        assert!(
            has_edge(&index, EdgeKind::HasType, "/use", target),
            "use has type {target}"
        );
    }
    assert!(has_edge(&index, EdgeKind::Reads, "/use", "/Mid/value"));
    assert!(has_edge(&index, EdgeKind::Reads, "/Box/run", "/Box/value"));
}

#[test]
fn test_typescript_variance_parameter_names_shadow_module_declarations() {
    // Each module declaration shares its name with a type parameter whose list holds
    // a variance annotation. A use of that name inside the declaration is the parameter.
    let source = "export type T = number;
export type O = string;
export interface Other { other: number }
export const out = 1;
export interface Held<out T = unknown> { held: T; plain: Other }
export interface Pair<in out O extends Other> { pair: O }
export class Kept<in T> { kept!: T; read(): T { return this.kept } }
export interface Plain { plain: T; second: O }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for (from, to) in [
        ("/Held/held", "/T"),
        ("/Held", "/T"),
        ("/Pair/pair", "/O"),
        ("/Pair", "/O"),
        ("/Kept/kept", "/T"),
        ("/Kept/read", "/T"),
    ] {
        assert!(
            !has_edge(&index, EdgeKind::HasType, from, to),
            "{from} must not use module {to}"
        );
    }
    assert!(!index.edges().edges().any(|edge| {
        matches!(
            edge.kind(),
            EdgeKind::Reads | EdgeKind::Writes | EdgeKind::HasType
        ) && edge.to().as_str().ends_with("/out")
    }));
    // Names that are not parameters keep resolving, inside and outside the declarations.
    assert!(has_edge(&index, EdgeKind::HasType, "/Held/plain", "/Other"));
    assert!(has_edge(&index, EdgeKind::HasType, "/Pair", "/Other"));
    assert!(has_edge(&index, EdgeKind::HasType, "/Plain/plain", "/T"));
    assert!(has_edge(&index, EdgeKind::HasType, "/Plain/second", "/O"));
}

fn calls(index: &WorkspaceIndex) -> Vec<(String, String)> {
    let mut calls = index
        .edges()
        .edges()
        .filter(|edge| edge.kind() == EdgeKind::Calls)
        .map(|edge| {
            let tail = |identity: &str| {
                identity
                    .split_once(".%2Fmain.")
                    .and_then(|(_, rest)| rest.split_once('/'))
                    .map_or_else(|| "<module>".to_owned(), |(_, name)| name.to_owned())
            };
            (tail(edge.from().as_str()), tail(edge.to().as_str()))
        })
        .collect::<Vec<_>>();
    calls.sort();
    calls.dedup();
    calls
}

#[test]
fn test_calls_in_deferred_initializers_belong_to_the_declaration_holding_them() {
    // `wrap(() => leaf())` defers the call: the declaration whose initializer holds that
    // function value is the caller, never the module or an enclosing callable.
    let source = "function leaf() { return 1 }
function wrap(run) { return run }
export const use = () => leaf();
export const built = wrap(() => { const inner = wrap(() => leaf()); return inner });
export const direct = leaf();
export function outer() { const local = wrap(() => leaf()); wrap(function () { leaf() }); return local }
export class Item { value = leaf(); held = wrap(() => leaf()); static made = wrap(() => leaf()); run() { return wrap(() => leaf()) } }
export const api = { member: wrap(() => leaf()), method() { return leaf() } };
";
    for path in ["main.mjs", "main.ts"] {
        let (_directory, index) = workspace_with_manifest(&[(path, source)], module_manifest());
        let found = calls(&index);
        let expect = |from: &str, to: &str| {
            assert!(
                found.contains(&(from.to_owned(), to.to_owned())),
                "{path}: missing {from} -> {to} in {found:?}"
            );
        };
        expect("use", "leaf");
        expect("built", "wrap");
        expect("built/inner", "leaf");
        // Module execution calls the direct initializers.
        expect("<module>", "leaf");
        expect("<module>", "wrap");
        expect("outer/local", "leaf");
        expect("outer", "leaf");
        expect("outer", "wrap");
        expect("Item/value", "leaf");
        expect("Item/held", "leaf");
        expect("Item/made", "leaf");
        expect("Item/run", "leaf");
        expect("api/member", "leaf");
        expect("api/method", "leaf");
        // Only the direct initializer of `direct` and the calls to `wrap` run with the module.
        assert_eq!(
            found
                .iter()
                .filter(|(from, to)| from == "<module>" && to == "leaf")
                .count(),
            1,
            "{path}: {found:?}"
        );
        assert!(
            !found.contains(&("built".to_owned(), "leaf".to_owned())),
            "{path}: {found:?}"
        );
    }
}

#[test]
fn tsx_callable_variables_keep_recursive_calls_and_closer_shadows() {
    let source = r#"export const Widget = (depth: number) => {
  if (depth > 0) return Widget(depth - 1);
  {
    const Widget = () => <span className="flex" />;
    return Widget();
  }
};
export const Shadowed = (Shadowed: () => JSX.Element) => Shadowed();
"#;
    let (_directory, index) = workspace_with_manifest(
        &[
            ("main.tsx", source),
            ("styles.css", "@import 'tailwindcss';"),
        ],
        r#"{"name":"fixture","version":"1.0.0","type":"module","devDependencies":{"tailwindcss":"4.1.0"}}"#,
    );
    let file = index
        .file(&ProjectPath::new("main.tsx").expect("fixture path"))
        .expect("indexed TSX source");
    assert!(
        file.syntax()
            .symbols()
            .iter()
            .any(|symbol| symbol.kind == "utility" && symbol.name == "flex")
    );
    assert_eq!(
        calls(&index),
        [
            ("Widget".to_owned(), "Widget".to_owned()),
            ("Widget".to_owned(), "Widget/Widget".to_owned()),
        ]
    );
}

#[test]
fn test_deferred_initializer_calls_keep_lexical_shadows_and_unknown_receivers_unresolved() {
    let source = "function leaf() { return 1 }
function wrap(run) { return run }
export const shadowed = wrap((leaf) => leaf());
export const local = wrap(() => { const leaf = () => 2; return leaf() });
export const unknown = wrap((value) => value.leaf());
";
    for path in ["main.mjs", "main.ts"] {
        let (_directory, index) = workspace_with_manifest(&[(path, source)], module_manifest());
        let found = calls(&index);
        // A parameter named like the function and an unknown receiver name no declaration.
        assert!(
            !found.contains(&("shadowed".to_owned(), "leaf".to_owned())),
            "{path}: {found:?}"
        );
        assert!(
            !found.contains(&("unknown".to_owned(), "leaf".to_owned())),
            "{path}: {found:?}"
        );
        // The local arrow is its own declaration; the module function is not called.
        assert!(
            found.contains(&("local".to_owned(), "local/leaf".to_owned())),
            "{path}: {found:?}"
        );
        assert!(
            !found.contains(&("local".to_owned(), "leaf".to_owned())),
            "{path}: {found:?}"
        );
    }
}

#[test]
fn test_overrides_need_an_inherited_member_and_never_a_constructor() {
    let source = "export interface Contract { run(): void; name: string }
export interface Wider extends Contract { run(): void }
export class Implemented implements Contract { name = ''; run() {} }
export class Parent { constructor() {} run() {} static make() {} }
export class Child extends Parent implements Contract {
  name = '';
  constructor() { super() }
  override run() {}
  static make() {}
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    let mut overrides = index
        .edges()
        .edges()
        .filter(|edge| edge.kind() == EdgeKind::Overrides)
        .map(|edge| {
            let tail = |identity: &str| {
                identity
                    .rsplit_once("main.ts/")
                    .expect("member")
                    .1
                    .to_owned()
            };
            (tail(edge.from().as_str()), tail(edge.to().as_str()))
        })
        .collect::<Vec<_>>();
    overrides.sort();
    overrides.dedup();
    assert_eq!(
        overrides,
        [
            ("Child/make".to_owned(), "Parent/make".to_owned()),
            ("Child/run".to_owned(), "Parent/run".to_owned()),
            ("Wider/run".to_owned(), "Contract/run".to_owned()),
        ]
    );
    assert!(has_edge(
        &index,
        EdgeKind::Implements,
        "/Implemented",
        "/Contract"
    ));
    assert!(has_edge(&index, EdgeKind::Extends, "/Child", "/Parent"));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Child/constructor",
        "/Parent"
    ));
}

#[test]
fn test_member_writes_refuse_calls_only_on_the_class_their_receiver_denotes() {
    // Each class calls its own `leaf` through `this`. A write refuses that call only when
    // it names that member on a receiver that denotes the class: `this` inside it, its
    // prototype, or a binding constructed as it. Writes through other objects name other
    // objects, and a computed key names no member at all.
    let source = "export class Item {
  leaf() { return 1 }
  use() { return this.leaf() }
  unrelated(target, key) { target[key] = 1; const made = {}; made[key] = 2; target.child.leaf = 3 }
}
export class Tagged { leaf() { return 1 } use() { return this.leaf() } tag() { this['label'] = 1 } }
export class Patched { leaf() { return 1 } use() { return this.leaf() } patch(key, value) { this[key] = value } }
export class Named { leaf() { return 1 } use() { return this.leaf() } patch(value) { this.leaf = value } }
export class Keyed { leaf() { return 1 } use() { return this.leaf() } }
Keyed.prototype['leaf'] = function () { return 2 };
export class Built { leaf() { return 1 } use() { return this.leaf() } }
const built = new Built();
built[Symbol.iterator] = 1;
export class Assigned { leaf() { return 1 } use() { return this.leaf() } }
const assigned = new Assigned();
assigned.leaf = () => 2;
export class Bound { constructor() { this.leaf = this.leaf.bind(this) } leaf() { return 1 } use() { return this.leaf() } }
export class Rebound { constructor(other) { this.leaf = other.leaf.bind(this) } leaf() { return 1 } use() { return this.leaf() } }
";
    for path in ["main.mjs", "main.ts"] {
        let (_directory, index) = workspace_with_manifest(&[(path, source)], module_manifest());
        let found = calls(&index);
        for class in ["Item", "Tagged", "Bound", "Patched", "Built"] {
            assert!(
                found.contains(&(format!("{class}/use"), format!("{class}/leaf"))),
                "{path}: {class} keeps its call in {found:?}"
            );
        }
        for class in ["Named", "Keyed", "Assigned", "Rebound"] {
            assert!(
                !found
                    .iter()
                    .any(|(from, _)| from == &format!("{class}/use")),
                "{path}: {class} must stay unresolved in {found:?}"
            );
        }
    }
}

#[test]
fn test_typescript_member_write_through_a_declared_class_binding_refuses_its_calls() {
    let source = "export class Typed { leaf() { return 1 } use() { return this.leaf() } }
export class Free { leaf() { return 1 } use() { return this.leaf() } }
export function patch(item: Typed, bag: Record<string, unknown>, key: string) { item.leaf = () => 1; bag[key] = 2; (bag as any).leaf = 3 }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    let found = calls(&index);
    assert!(
        !found.iter().any(|(from, _)| from == "Typed/use"),
        "{found:?}"
    );
    assert!(
        found.contains(&("Free/use".to_owned(), "Free/leaf".to_owned())),
        "{found:?}"
    );
}

#[test]
fn test_typescript_calls_through_declared_callable_and_constructible_bindings() {
    // A binding whose declared type is a function type alias, or an interface with a call
    // or construct signature, is the target of a call or `new` on it. The type and the
    // value named `$constructor` are separate declarations.
    let core = "export interface $constructor<T, D = unknown> { new (def: D): T; init(inst: T, def: D): void }
export function $constructor<T>(name: string): $constructor<T> { return null as never }
export type Callback = (value: string) => number;
export type Renamed = Callback;
export interface Callable { (value: string): number; extra: string }
export interface Shape { extra: string }
";
    let source = "import * as core from './core.js';
import type { Callback, Callable, Renamed, Shape } from './core.js';
export interface Made { kind: 'made' }
export const Made: core.$constructor<Made> = core.$constructor('Made');
export const plain = core.$constructor('Plain');
declare const callback: Callback;
declare const renamed: Renamed;
declare const callable: Callable;
declare const shape: Shape;
declare const unknown: unknown;
export function make() { return new Made({}) }
export function run() { callback('a'); renamed('b'); return callable('c') }
export function refuse() { new plain({}); new callback(); callable.extra; shape('d'); new shape(); return unknown('e') }
";
    let (_directory, index) =
        workspace_with_manifest(&[("main.ts", source), ("core.ts", core)], module_manifest());
    let found = calls(&index);
    for (from, to) in [
        ("make", "Made"),
        ("run", "callback"),
        ("run", "renamed"),
        ("run", "callable"),
    ] {
        assert!(
            found.contains(&(from.to_owned(), to.to_owned())),
            "missing {from} -> {to} in {found:?}"
        );
    }
    // An inferred value, a callable used with `new`, a shape without a signature and an
    // unknown value name no call target.
    assert!(!found.iter().any(|(from, _)| from == "refuse"), "{found:?}");
    assert!(has_edge(
        &index,
        EdgeKind::HasType,
        "/Made",
        "core.ts/$constructor"
    ));
}

#[test]
fn test_typescript_root_parse_error_keeps_the_declarations_outside_its_body() {
    // The interface body cannot be parsed and turns the document root into an error.
    let respond = "export type Before = string
export interface Respond {
  <T extends Before, U extends number>(
    data: T,
    status?: U
  ): Wrapped<T, U, 'body'>
  <T extends Before, U extends number>(
    data: T,
    init?: Init<U>
  ): Wrapped<T, U, 'body'>
  <T extends null, U extends number>(
    data: T,
    status?: U
  ): Wrapped<null, U, 'body'>
}
export class Item {
  value = 1
  read() { return this.value }
}
export function later(item: Item) { return item.read() }
";
    let main = "import { Item, later } from './respond.js'
import type { Respond } from './respond.js'
export function run(item: Item, respond: Respond) { return later(item) }
";
    let (_directory, index) = workspace_with_manifest(
        &[("main.ts", main), ("respond.ts", respond)],
        module_manifest(),
    );
    let language =
        rift_protocol::read::Language::from_identity_segment("typescript").expect("language");
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("identity coverage")
    );
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/run",
        "respond.ts/later"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "respond.ts/later",
        "respond.ts/Item/read"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "respond.ts/Item/read",
        "respond.ts/Item/value"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Contains,
        "respond.ts/Item",
        "respond.ts/Item/read"
    ));
    for target in ["respond.ts/Item", "respond.ts/Respond"] {
        assert!(
            has_edge(&index, EdgeKind::HasType, "main.ts/run", target),
            "{target}"
        );
    }
    // Nothing is declared from inside the body that was left out.
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Contains && edge.from().as_str().ends_with("respond.ts/Respond")
    }));
}

#[test]
fn test_typescript_declaration_files_resolve_unique_extensionless_type_imports() {
    let directory = tempfile::tempdir().expect("workspace");
    let source = "import type { Direct } from './direct';
import type { Via } from './barrel';
import type { Preferred } from './preferred';
import type { Ambiguous } from './ambiguous';
import type { Mixed } from './mixed';
export type Selected = Direct | Via | Preferred;
export type Unknown = Ambiguous | Mixed;
export type Shadow<Direct> = Direct;
";
    for (path, text) in [
        ("package.json", module_manifest()),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"moduleResolution":"bundler"}}"#,
        ),
        ("main.d.ts", source),
        ("direct.d.ts", "export type Direct = string;"),
        ("barrel/index.d.ts", "export type { Via } from './item';"),
        ("barrel/item.d.ts", "export type Via = number;"),
        ("preferred.ts", "export type Preferred = string;"),
        ("preferred.d.ts", "export type Preferred = number;"),
        ("ambiguous.d.ts", "export type Ambiguous = number;"),
        ("ambiguous/index.d.ts", "export type Ambiguous = string;"),
        ("mixed.ts", "export type Mixed = number;"),
        ("mixed.tsx", "export type Mixed = string;"),
        ("mixed.d.ts", "export type Mixed = boolean;"),
        ("ambient.d.ts", "export declare function run(): void;"),
        (
            "runtime.ts",
            "import { run } from './ambient'; export function use() { run(); }",
        ),
    ] {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("fixture");
    }
    let index = WorkspaceIndex::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    for (target, name, has_type) in [
        ("direct.d.ts/Direct", "Direct", true),
        ("barrel%2Fitem.d.ts/Via", "Via", true),
        ("preferred.ts/Preferred", "Preferred", true),
        ("direct.d.ts", "Direct", false),
        ("barrel%2Findex.d.ts", "Via", false),
    ] {
        assert!(
            index.edges().edges().any(|edge| {
                let range = edge.occurrence().range();
                let start = usize::try_from(range.start()).expect("import start");
                let end = usize::try_from(range.end()).expect("import end");
                edge.kind() == EdgeKind::Imports
                    && edge.from().as_str().ends_with("main.d.ts")
                    && edge.to().as_str().ends_with(target)
                    && &source[start..end] == name
            }),
            "import {target}"
        );
        if has_type {
            assert!(has_edge(
                &index,
                EdgeKind::HasType,
                "main.d.ts/Selected",
                target
            ));
        }
    }
    assert!(!index.edges().edges().any(|edge| {
        (edge.from().as_str().ends_with("main.d.ts")
            && edge.kind() == EdgeKind::Imports
            && ["/Ambiguous", "/Mixed", "preferred.d.ts/Preferred"]
                .iter()
                .any(|wrong| edge.to().as_str().ends_with(wrong)))
            || (edge.from().as_str().ends_with("main.d.ts/Shadow")
                && edge.kind() == EdgeKind::HasType
                && edge.to().as_str().ends_with("direct.d.ts/Direct"))
            || (edge.from().as_str().ends_with("runtime.ts/use")
                && edge.kind() == EdgeKind::Calls
                && edge.to().as_str().ends_with("ambient.d.ts/run"))
    }));
}

#[test]
fn test_typescript_type_star_reexports_keep_the_written_modules_and_selected_type() {
    let star = "export type * from './leaf';";
    let (_directory, index) = workspace_with_manifest(
        &[
            (
                "main.d.ts",
                "import type { Item } from './barrel'; export type Selected = Item;",
            ),
            ("barrel.d.ts", star),
            ("leaf.d.ts", "export type Item = string;"),
        ],
        module_manifest(),
    );
    for (kind, from, to) in [
        (EdgeKind::Imports, "main.d.ts", "barrel.d.ts"),
        (EdgeKind::Imports, "main.d.ts", "leaf.d.ts/Item"),
        (EdgeKind::HasType, "main.d.ts/Selected", "leaf.d.ts/Item"),
    ] {
        assert!(has_edge(&index, kind, from, to), "{kind:?} {from} {to}");
    }
    let written = index
        .edges()
        .edges()
        .find(|edge| {
            edge.kind() == EdgeKind::Imports
                && edge.from().as_str().ends_with("barrel.d.ts")
                && edge.to().as_str().ends_with("leaf.d.ts")
        })
        .expect("written type-only star module edge");
    assert_eq!(written.occurrence().unit().key().as_str(), "barrel.d.ts");
    assert_eq!(written.occurrence().range().start(), 0);
    assert_eq!(
        written.occurrence().range().end(),
        u64::try_from(star.len()).expect("source length")
    );
}

#[test]
fn test_typescript_type_star_reexports_refuse_value_lookup_and_unresolved_members() {
    let (_directory, index) = workspace_with_manifest(
        &[
            (
                "main.ts",
                "import { run as ordinary } from './plain.ts'; import { run as typed } from './typed.ts'; import { tools } from './namespace.ts'; export function keep() { ordinary(); tools.run(); } export function refuse() { typed(); }",
            ),
            ("runtime.ts", "export function run() {}"),
            ("plain.ts", "export * from './runtime.ts';"),
            ("namespace.ts", "export * as tools from './runtime.ts';"),
            ("typed.ts", "export type * from './runtime.ts';"),
            (
                "unknown.d.ts",
                "import type { Item } from './ambiguous'; import type { Absent } from './cycle-a'; import type { Kept } from './shadow'; export type Ambiguous = Item; export type Cyclic = Absent; export type Shadowed = Kept;",
            ),
            (
                "shadow.d.ts",
                "export type * from './other'; export type Kept = string;",
            ),
            ("other.d.ts", "export type Kept = number;"),
            (
                "ambiguous.d.ts",
                "export type * from './first'; export type * from './second';",
            ),
            ("first.d.ts", "export type Item = string;"),
            ("second.d.ts", "export type Item = number;"),
            ("cycle-a.d.ts", "export type * from './cycle-b';"),
            ("cycle-b.d.ts", "export type * from './cycle-a';"),
        ],
        module_manifest(),
    );
    assert_eq!(
        index
            .edges()
            .edges()
            .filter(|edge| {
                edge.kind() == EdgeKind::Calls
                    && edge.from().as_str().ends_with("main.ts/keep")
                    && edge.to().as_str().ends_with("runtime.ts/run")
            })
            .count(),
        2
    );
    assert!(has_edge(
        &index,
        EdgeKind::HasType,
        "unknown.d.ts/Shadowed",
        "shadow.d.ts/Kept"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::HasType,
        "unknown.d.ts/Shadowed",
        "other.d.ts/Kept"
    ));
    assert!(!index.edges().edges().any(|edge| {
        (edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("main.ts/refuse"))
            || (edge.kind() == EdgeKind::HasType
                && ["unknown.d.ts/Ambiguous", "unknown.d.ts/Cyclic"]
                    .iter()
                    .any(|owner| edge.from().as_str().ends_with(owner)))
    }));
}

#[test]
fn test_typescript_function_value_keeps_its_identity_through_a_body_error() {
    // The grammar reads `global` as a keyword: its use as a value is a parse error
    // inside the arrow body. The variable is still named by its own header.
    let source = "export const inspect = () => {
  const global = globalThis as any
  return global?.process?.env
}
export function run() { return inspect() }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/run",
        "main.ts/inspect"
    ));
    let view = index.symbol_view().expect("view");
    assert!(view.normalized_graph().records().iter().any(|record| {
        record
            .identity()
            .is_some_and(|identity| identity.as_str().ends_with("main.ts/inspect"))
    }));
}

#[test]
fn test_typescript_members_through_declared_getters_and_structural_aliases() {
    let source = "export class Request { raw = 1; add(value: number) { return value } }
export class Context {
  #req = new Request()
  get req(): Request { return this.#req }
  get loose() { return this.#req }
}
export type Options = { manifest?: string; nested?: { deep: number } }
export type Either = Options | Request
export function conn(c: Context) { c.req.add(1); return c.req.raw }
export function assets(options: Options) { return [options.manifest, options?.nested] }
export function refuse(c: Context, either: Either) { return [c.loose.raw, either.manifest, c.req.missing] }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(&index, EdgeKind::Calls, "/conn", "/Request/add"));
    assert!(has_edge(&index, EdgeKind::Reads, "/conn", "/Request/raw"));
    assert!(has_edge(&index, EdgeKind::Reads, "/conn", "/Context/req"));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "/assets",
        "/Options/manifest"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "/assets",
        "/Options/nested"
    ));
    // A getter without a declared return type, a union alias and an absent member name
    // no declaration.
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "/refuse",
        "/Context/loose"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Reads
            && edge.from().as_str().ends_with("/refuse")
            && ["/raw", "/manifest", "/missing"]
                .iter()
                .any(|name| edge.to().as_str().ends_with(name))
    }));
}

fn call_count(index: &WorkspaceIndex, from: &str, to: &str) -> usize {
    index
        .edges()
        .edges()
        .filter(|edge| {
            edge.kind() == EdgeKind::Calls
                && edge.from().as_str().ends_with(from)
                && edge.to().as_str().ends_with(to)
        })
        .count()
}

const CALLBACK_LIBRARY: &str = "export class Request { raw = 1 }
export class Context { req: Request = new Request(); text(body: string) { return body } }
export class Doc {
  text = ''
  write(text: string) { this.text = text }
  indented(run: (doc: Doc) => void) { run(this) }
  both(first: number, run: (count: number, doc: Doc) => void) { run(first, this) }
  over(run: (doc: Doc) => void): void
  over(run: (text: string) => void, extra: number): void
  over(run: unknown) {}
}
export type Next = () => Promise<void>
export type Handler = (c: Context, next: Next) => unknown
export type Renamed = Handler
export interface Shape { value: number }
export function create(handler: Handler) { return handler }
export function plain(handler) { return handler }
export function generic<T extends Context>(run: (value: T) => void) { return run }
";

#[test]
fn test_typescript_callback_parameters_take_the_written_type_at_their_position() {
    // An unannotated callback parameter is typed by the function type written for it: the
    // declared type of the binding the function initializes, the declared parameter of the
    // resolved callee, or the declared return type of the function that returns it.
    let main = "import { Doc, create } from './lib.js'
import type { Handler, Renamed } from './lib.js'
export function emit(doc: Doc) {
  doc.indented((d) => { d.write('a'); d.indented((inner) => inner.write('b')) })
  doc.both(1, (count, second) => second.write('c'))
}
export const handle: Handler = async (c, next) => { c.text('a'); return c.req.raw }
export const renamed: Renamed = (c) => c.text('b')
export const made = create(async (c) => c.text('c'))
export const factory = (): Handler => { return async function inner(ctx) { return ctx.text('d') } }
export const concise = (): Handler => (ctx) => ctx.text('e')
";
    let (_directory, index) = workspace_with_manifest(
        &[("main.ts", main), ("lib.ts", CALLBACK_LIBRARY)],
        module_manifest(),
    );
    assert_eq!(call_count(&index, "main.ts/emit", "lib.ts/Doc/write"), 3);
    assert_eq!(call_count(&index, "main.ts/emit", "lib.ts/Doc/indented"), 2);
    // A function expression returned by `factory` is no declaration: `factory` holds it.
    for from in ["handle", "renamed", "made", "factory", "concise"] {
        assert_eq!(
            call_count(&index, &format!("main.ts/{from}"), "lib.ts/Context/text"),
            1,
            "{from} in {:?}",
            calls(&index)
        );
    }
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/handle",
        "lib.ts/Context/req"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/handle",
        "lib.ts/Request/raw"
    ));
}

#[test]
fn test_callback_parameters_without_a_written_type_stay_unresolved() {
    let main = "import { Doc, generic, plain } from './lib.js'
import type { Context, Shape } from './lib.js'
export function refuse(doc: Doc) {
  plain((c) => c.text('callee parameter has no type'))
  doc.over((d) => d.write('callee is overloaded'))
  missing((c) => c.text('callee is not resolved'))
  const loose = (c) => c.text('binding has no type')
  const shaped: Shape = (c) => c.text('type is no function type')
  doc.both(1, (count) => count.write('parameter type is no declaration'))
  doc.indented((first, second) => second.write('no parameter at that position'))
  generic((c) => c.text('the call decides a type parameter'))
  doc.indented((d: Context) => d.text('its own annotation decides'))
  return [loose, shaped]
}
";
    let (_directory, index) = workspace_with_manifest(
        &[("main.ts", main), ("lib.ts", CALLBACK_LIBRARY)],
        module_manifest(),
    );
    assert_eq!(call_count(&index, "main.ts/refuse", "lib.ts/Doc/write"), 0);
    assert_eq!(
        call_count(&index, "main.ts/refuse/loose", "lib.ts/Context/text"),
        0
    );
    assert_eq!(
        call_count(&index, "main.ts/refuse/shaped", "lib.ts/Context/text"),
        0
    );
    // Only the callback with its own annotation names the member.
    assert_eq!(
        index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Calls
                && edge.to().as_str().ends_with("lib.ts/Context/text"))
            .count(),
        1
    );
    assert_eq!(
        call_count(&index, "main.ts/refuse", "lib.ts/Context/text"),
        1
    );
    // JavaScript writes no types, so the same shapes name nothing there.
    let library = "export class Doc { write(text) {} indented(run) { run(this) } }";
    let script = "import { Doc } from './lib.mjs'
export function emit() { const doc = new Doc(); doc.indented((d) => d.write('a')) }
";
    let (_directory, index) = workspace_with_manifest(
        &[("main.mjs", script), ("lib.mjs", library)],
        module_manifest(),
    );
    assert_eq!(
        call_count(&index, "main.mjs/emit", "lib.mjs/Doc/indented"),
        1
    );
    assert_eq!(call_count(&index, "main.mjs/emit", "lib.mjs/Doc/write"), 0);
}

#[test]
fn test_typescript_function_values_behind_assertions_stay_callable() {
    // A function written under a type assertion, `as`, `satisfies` or a non-null mark is
    // the same function value. The grammar also reads a generic arrow that follows a parse
    // error as a type assertion over the arrow.
    let source = "export type Handler = () => number
export const cast = <Handler>function () { return 1 }
export const named = (() => 2) as Handler
export const checked = (() => 3) satisfies Handler
export const present = (() => 4)!
export const plain = <Handler>(1 as unknown)
export function run() { return cast() + named() + checked() + present() }
export function refuse() { return plain() }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for target in ["cast", "named", "checked", "present"] {
        assert_eq!(
            call_count(&index, "main.ts/run", &format!("main.ts/{target}")),
            1,
            "{target} in {:?}",
            calls(&index)
        );
    }
    // An assertion over a value that is no function names no callable.
    assert_eq!(call_count(&index, "main.ts/refuse", "main.ts/plain"), 0);
}

#[test]
fn test_this_members_resolve_inside_object_literals_beside_a_declared_shape() {
    // The method declares its return shape and returns an object literal of that shape:
    // two members named `ctx` share one qualified name. `this` still belongs to the class
    // member that holds the literal.
    let source = "export class Status { ok = true }
export class Base {
  _def = 1
  static shared = 2
  typed(): { status: Status; ctx: { data: number } } {
    return { status: new Status(), ctx: { data: this._def } }
  }
  static make(): { ctx: { data: number } } { return { ctx: { data: this.shared } } }
  wrong(): { ctx: { data: number } } {
    function inner() { return { ctx: { data: this._def } } }
    return inner()
  }
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    let reads = |holder: &str, member: &str| {
        index.edges().edges().any(|edge| {
            edge.kind() == EdgeKind::Reads
                && edge.from().as_str().contains(holder)
                && edge.to().as_str().ends_with(member)
        })
    };
    assert!(reads("/Base/typed", "/Base/_def"));
    assert!(reads("/Base/make", "/Base/shared"));
    // A nested function has its own receiver: nothing is read from the class there.
    assert!(!reads("/Base/wrong", "/Base/_def"));
    assert!(!reads("/Base/make", "/Base/_def"));
}

#[test]
fn test_typescript_members_through_type_parameter_constraints() {
    // A value typed by a type parameter has at least the members its declared constraint
    // names. An unconstrained parameter and a constraint that is no declaration name none,
    // and the parameter still shadows a module declaration of its own name.
    let source = "export interface Def { errorMap?: string; describe(): string }
export interface D { other: number }
export class Base<Output = unknown, D extends Def = Def> {
  readonly _def!: D
  read() { return [this._def.errorMap, this._def.describe()] }
  wrong() { return this._def.other }
}
export function pick<T extends Def>(value: T) { return value.errorMap }
export function loose<T>(value: T) { return value.errorMap }
export function shaped<T extends { errorMap: string }>(value: T) { return value.errorMap }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "/Base/read",
        "/Def/errorMap"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Base/read",
        "/Def/describe"
    ));
    assert!(has_edge(&index, EdgeKind::Reads, "/pick", "/Def/errorMap"));
    for from in ["/loose", "/shaped", "/Base/wrong"] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Reads
                    && edge.from().as_str().ends_with(from)
                    && (edge.to().as_str().ends_with("/errorMap")
                        || edge.to().as_str().ends_with("/other"))
            }),
            "{from}"
        );
    }
}

#[test]
fn test_typescript_members_of_values_destructured_from_declared_returns() {
    // A name destructured from a call takes the type its key has in the callee's declared
    // return type: an inline object type, or a named type that declares the member.
    let source = "export class Status { value = 'ok'; dirty() { this.value = 'dirty' } }
export interface Context { parsedType: string; data: unknown }
export interface Parts { status: Status; ctx: Context }
export class Base {
  process(input: string): { status: Status; ctx: Context } { return { status: new Status(), ctx: { parsedType: input, data: 1 } } }
  named(input: string): Parts { return this.process(input) }
  loose(input: string) { return this.process(input) }
  run(input: string) {
    const { status, ctx } = this.process(input)
    status.dirty()
    return ctx.parsedType
  }
  renamed(input: string) {
    const { ctx: inner, status } = this.named(input)
    status.dirty()
    return inner.data
  }
  refuse(input: string, other: { ctx: Context }) {
    const { ctx } = this.loose(input)
    const { ctx: unknown } = missing(input)
    const { absent } = this.process(input)
    return [ctx.parsedType, unknown.parsedType, absent.parsedType, other]
  }
}
export function free(base: Base) { const { ctx } = base.process('a'); return ctx.data }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Base/run",
        "/Status/dirty"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "/Base/run",
        "/Context/parsedType"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Base/renamed",
        "/Status/dirty"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "/Base/renamed",
        "/Context/data"
    ));
    assert!(has_edge(&index, EdgeKind::Reads, "/free", "/Context/data"));
    // No declared return type, an unresolved callee and a key the type does not declare.
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Reads
            && edge.from().as_str().ends_with("/Base/refuse")
            && edge.to().as_str().ends_with("/parsedType")
    }));
}

#[test]
fn test_typescript_module_augmentation_keeps_the_file_imports() {
    // A file that augments another module is still a module of its own: the augmentation
    // declares into the other module, and the file's imports stay the file's.
    let main = "import type { Base } from './other.ts'
import { value } from './other.ts'
declare module './other' {
  interface Added { added: Base }
}
export const use: Base = value
";
    let other = "export interface Base { base: number }
export const value: Base = { base: 1 }
";
    let (_directory, index) =
        workspace_with_manifest(&[("main.ts", main), ("other.ts", other)], module_manifest());
    for target in ["other.ts/Base", "other.ts/value"] {
        assert!(
            index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Imports
                    && edge.from().as_str().ends_with(".%2Fmain.ts")
                    && edge.to().as_str().ends_with(target)
            }),
            "{target}"
        );
    }
    assert!(has_edge(
        &index,
        EdgeKind::HasType,
        "main.ts/use",
        "other.ts/Base"
    ));
}

#[test]
fn test_typescript_members_of_values_bound_to_declared_call_results() {
    // A name bound to a call takes the named type the resolved callee declares as its
    // return type.
    let source = "export interface Context { parsedType: string; data: unknown }
export class Status { dirty() { return 1 } }
export function make(input: string): Context { return { parsedType: input, data: 1 } }
export function pick(input: string): Context
export function pick(input: number): Status
export function pick(input: unknown): unknown { return input }
export class Base {
  context(input: string): Context { return make(input) }
  loose(input: string) { return make(input) }
  status(): Status { return new Status() }
  run(input: string) {
    const ctx = this.context(input)
    const status = this.status()
    status.dirty()
    return ctx.parsedType
  }
  refuse(input: string, other: Context) {
    const loose = this.loose(input)
    const unknown = missing(input)
    const picked = pick(input)
    let moved = this.context(input)
    moved = other
    return [loose.parsedType, unknown.parsedType, picked.parsedType, moved.parsedType]
  }
}
export function free() { const ctx = make('a'); return ctx.data }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Base/run",
        "/Status/dirty"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "/Base/run",
        "/Context/parsedType"
    ));
    assert!(has_edge(&index, EdgeKind::Reads, "/free", "/Context/data"));
    // No declared return type, an unresolved callee, an overloaded callee and a name
    // assigned again.
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Reads
            && edge.from().as_str().ends_with("/Base/refuse")
            && edge.to().as_str().ends_with("/parsedType")
    }));
}

#[test]
fn test_typescript_imports_through_a_reexport_keep_the_written_module() {
    // An import names the module it is written against. When the name is declared in
    // another file, reached through a reexport, the terminal declaration and the written
    // module are both stored.
    let bridge = "export type { Item } from './leaf.ts'
export { missing } from './absent.ts'
";
    let leaf = "export interface Item { value: number }
export class Value {}
";
    let probe = "import type { Item } from './bridge.ts'
import { Value } from './leaf.ts'
import { missing } from './bridge.ts'
import { other } from './nowhere.ts'
export function use(value: Item) { return [new Value(), missing, other] }
";
    let (_directory, index) = workspace_with_manifest(
        &[
            ("bridge.ts", bridge),
            ("leaf.ts", leaf),
            ("probe.ts", probe),
        ],
        module_manifest(),
    );
    let imports = |to: &str| {
        index
            .edges()
            .edges()
            .filter(|edge| {
                edge.kind() == EdgeKind::Imports
                    && edge.from().as_str().ends_with(".%2Fprobe.ts")
                    && edge.to().as_str().ends_with(to)
            })
            .count()
    };
    assert_eq!(imports("leaf.ts/Item"), 1);
    assert_eq!(imports(".%2Fbridge.ts"), 1);
    assert_eq!(imports("leaf.ts/Value"), 1);
    // A name declared in the written module adds no module edge, and neither does a name
    // or a module that resolves to nothing.
    assert_eq!(imports(".%2Fleaf.ts"), 0);
    assert_eq!(imports("nowhere.ts"), 0);
}

#[test]
fn test_commonjs_names_destructured_from_require_import_their_exports() {
    // An object pattern over `require` binds each name to the export its key names, as
    // the member form `require('./lib').name` does. An export assigned from a name is the
    // declaration that name binds, and a required name exported again is a reexport.
    let library = "class Item { run() { return 1 } }
function make(name) { return new Item(name) }
function hidden() { return 2 }
let moved = function () { return 3 }
moved = hidden
exports.Item = Item;
exports.make = make;
exports.moved = moved;
";
    let index_file = "const { Item, make } = require('./lib.js');
exports.Item = Item;
module.exports.build = make;
";
    let main = "const { Item, make: build } = require('./lib.js');
const { absent } = require('./nowhere.js');
const { Item: Deep = null, ...rest } = require('./lib.js');
const whole = require('./lib.js');
const Picked = require('./lib.js').Item;
const entry = require('./index.js');
function use() { const item = new Item(); return [build('a'), item.run(), absent, rest, Deep] }
function through() { return [whole.make('b'), new Picked(), new entry.Item(), entry.build('c')] }
function shadowed(require) { const { make } = require('./lib.js'); return make('b') }
function replaced() { return whole.moved() }
exports.use = use;
";
    let (_directory, index) = workspace(&[
        ("lib.js", library),
        ("index.js", index_file),
        ("main.js", main),
    ]);
    // The import belongs to the name it binds, as in the member form.
    for (name, target) in [
        ("main.js/Item", "lib.js/Item"),
        ("main.js/build", "lib.js/make"),
    ] {
        assert!(has_edge(&index, EdgeKind::Imports, name, target), "{name}");
    }
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "lib.js/make"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "lib.js/Item"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "lib.js/Item/run"
    ));
    assert_eq!(call_count(&index, "main.js/through", "lib.js/make"), 2);
    assert_eq!(call_count(&index, "main.js/through", "lib.js/Item"), 2);
    // A pattern with a default or a rest entry binds no export, a `require` that names a
    // parameter binds nothing, and an export assigned from a name written again keeps no
    // call.
    for caller in ["main.js/shadowed", "main.js/replaced"] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with(caller)
            }),
            "{caller}"
        );
    }
}

#[test]
fn test_commonjs_named_function_assigned_to_module_exports_keeps_its_name() {
    // A named function expression assigned to `module.exports` is declared under the name
    // it is written with, and the module's default export is that declaration. A function
    // without a name keeps the export name.
    let library = "function leaf() { return 1 }
module.exports = function bind(fn) { return leaf(fn) };
";
    let anonymous = "module.exports = function (value) { return value };
";
    let main = "const bind = require('./lib.js');
const plain = require('./anonymous.js');
function use() { return [bind(1), plain(2)] }
exports.use = use;
";
    let (_directory, index) = workspace(&[
        ("lib.js", library),
        ("anonymous.js", anonymous),
        ("main.js", main),
    ]);
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "lib.js/bind",
        "lib.js/leaf"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "lib.js/bind"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "anonymous.js/default"
    ));
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.to().as_str().ends_with("lib.js/default"))
    );
    let language =
        rift_protocol::read::Language::from_identity_segment("javascript").expect("language");
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("identity coverage")
    );
}

#[test]
fn test_javascript_documentation_comment_types_type_parameters_and_returns() {
    // In JavaScript a documentation comment written right before a function states the
    // types of its parameters and of its result. A named type is a class in scope, or a
    // type the comment imports from another module.
    let option = "class Option {
  name() { return 'o' }
  attributeName() { return 'a' }
}
exports.Option = Option;
";
    let help = "class Help { format() { return '' } }
exports.Help = Help;
";
    let command = "const { Option } = require('./option.js');
/**
 * @typedef { import(\"./help.js\").Help } Help
 */
class Command {
  /**
   * @param {Option} option
   * @param {string} label
   * @returns {Command}
   */
  addOption(option, label) {
    option.name();
    return this;
  }

  /** @return {Option} */
  createOption() { return new Option() }

  /**
   * @param {(Option | null | undefined)} [maybe]
   */

  optional(maybe) { return maybe.attributeName() }

  use() {
    const created = this.createOption();
    return created.attributeName();
  }

  /** @param {Help} helper */
  help(helper) { return helper.format() }

  /** @param {Option} other */
  renamed(option) { return option.name() }

  /** @param {Option[]} list */
  many(list) { return list.name() }

  /** @param {Missing} option */
  unknown(option) { return option.name() }
}
/** @param {Option} option */
const gap = 1;
function far(option) { return option.name() }
exports.Command = Command;
exports.far = far;
exports.gap = gap;
";
    let (_directory, index) = workspace(&[
        ("option.js", option),
        ("help.js", help),
        ("command.js", command),
    ]);
    for (from, to) in [
        ("command.js/Command/addOption", "option.js/Option/name"),
        (
            "command.js/Command/optional",
            "option.js/Option/attributeName",
        ),
        ("command.js/Command/use", "option.js/Option/attributeName"),
        ("command.js/Command/help", "help.js/Help/format"),
    ] {
        assert!(
            has_edge(&index, EdgeKind::Calls, from, to),
            "{from} -> {to}"
        );
    }
    // A tag naming another parameter, a type that is not a single named type, a name with
    // no declaration and a comment separated from the function by a statement type
    // nothing.
    for caller in [
        "command.js/Command/renamed",
        "command.js/Command/many",
        "command.js/Command/unknown",
        "command.js/far",
    ] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with(caller)
            }),
            "{caller}"
        );
    }
}

#[test]
fn test_typescript_ignores_documentation_comment_types() {
    // TypeScript reads types from annotations only; a documentation comment types nothing.
    let source = "export class Item { run() { return 1 } }
/** @param {Item} item */
export function use(item) { return item.run() }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("main.ts/use")
    }));
}

#[test]
fn test_commonjs_module_exports_of_a_declared_name_exports_that_declaration() {
    // `module.exports = name` makes the declaration `name` binds the module's value, as
    // `export default name` does for a module. A module that assigns its value twice
    // exports nothing a call can trust; a name given only functions stays callable (D35).
    // `new` constructs a function written with the `function` keyword, never an arrow
    // function.
    let error = "function AxiosError(message) { this.message = message }
AxiosError.prototype.toJSON = function toJSON() { return {} };
module.exports = AxiosError;
";
    let value = "'use strict';
function sanitize(value) { return value }
module.exports = sanitize;
";
    let replaced = "function first() { return 1 }
function second() { return 2 }
module.exports = first;
module.exports = second;
";
    let moved = "let moved = function () { return 1 };
moved = function () { return 2 };
module.exports = moved;
";
    let main = "const AxiosError = require('./error.js');
var sanitize = require('./value');
const replaced = require('./replaced.js');
const moved = require('./moved.js');
function use() { sanitize(1); return new AxiosError('x') }
const arrow = () => 1;
function refuse() { return [replaced(), new arrow()] }
function given() { return moved() }
function shadowed(require) { const local = require('./value'); return local(1) }
exports.use = use;
exports.refuse = refuse;
exports.shadowed = shadowed;
exports.given = given;
";
    let (_directory, index) = workspace(&[
        ("error.js", error),
        ("value.js", value),
        ("replaced.js", replaced),
        ("moved.js", moved),
        ("main.js", main),
    ]);
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "value.js/sanitize"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "error.js/AxiosError"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/given",
        "moved.js/moved"
    ));
    for caller in ["main.js/refuse", "main.js/shadowed"] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with(caller)
            }),
            "{caller}"
        );
    }
}

#[test]
fn test_typescript_nullable_declared_types_type_their_values() {
    // A written type that is one named type in a union with `null` or `undefined` declares
    // that named type, for a parameter, a variable and a declared return type.
    let source = "export interface StateStorage {
  getItem: (name: string) => string | null
  setItem(name: string, value: string): void
}
export class Holder { value = 1 }
export function union(storage: StateStorage | undefined, holder?: (Holder | null)) {
  storage.getItem('a')
  storage.setItem('a', 'b')
  return holder.value
}
export function create(): StateStorage | null | undefined { return undefined }
export function result() { const s = create(); return s.getItem('b') }
let kept: undefined | StateStorage
export function later() { return kept.getItem('c') }
export class Other { getItem() { return 1 } }
export function reassigned(storage: StateStorage | undefined, other: StateStorage) {
  storage = other
  return storage.getItem('d')
}
export function refuse(either: StateStorage | Other, list: StateStorage[] | null) {
  let built = new Other()
  built = either as Other
  return [either.getItem('x'), list.getItem('y'), built.getItem('z')]
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for (from, to) in [
        ("main.ts/union", "main.ts/StateStorage/getItem"),
        ("main.ts/union", "main.ts/StateStorage/setItem"),
        ("main.ts/result", "main.ts/StateStorage/getItem"),
        ("main.ts/later", "main.ts/StateStorage/getItem"),
        ("main.ts/reassigned", "main.ts/StateStorage/getItem"),
    ] {
        assert!(
            has_edge(&index, EdgeKind::Calls, from, to),
            "{from} -> {to}"
        );
    }
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/union",
        "main.ts/Holder/value"
    ));
    // Two named types and an array in a union type nothing, and a binding typed only by
    // its initializer loses that type when it is assigned again.
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && edge.from().as_str().ends_with("main.ts/refuse")
            && edge.to().as_str().ends_with("/getItem")
    }));
}

#[test]
fn test_calls_through_bindings_of_declared_callable_results_and_other_writes() {
    // A name bound to a call result is callable when the resolved callee declares a
    // callable return type; the call targets the binding, never a function body. A write
    // refuses only the binding it writes: a write to an inner binding of the same name
    // leaves the outer one callable.
    let source = "export type Handler = (value: number) => number
export interface Signature { (value: number): number }
export function some(): Handler { return (value) => value }
export function signed(): Signature { return (value) => value }
export function plain() { return (value: number) => value }
export function pick<T>(value: T): T { return value }
export function use() {
  const handler = some()
  const other = signed()
  return [handler(1), other(2)]
}
export function inner() {
  const check = () => helper(1)
  const helper = (value: number) => {
    let helper
    helper = value
    return helper
  }
  return check()
}
export function refuse() {
  const loose = plain()
  const generic = pick(() => 1)
  let moved = some()
  moved = plain()
  return [loose(1), generic(), moved(2)]
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for (from, to) in [
        ("main.ts/use", "main.ts/use/handler"),
        ("main.ts/use", "main.ts/use/other"),
        ("main.ts/inner/check", "main.ts/inner/helper"),
    ] {
        assert!(
            has_edge(&index, EdgeKind::Calls, from, to),
            "{from} -> {to}"
        );
    }
    for name in ["loose", "generic", "moved"] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Calls
                    && edge.to().as_str().ends_with(&format!("/refuse/{name}"))
            }),
            "{name}"
        );
    }
}

#[test]
fn test_members_of_call_results_through_declared_return_types() {
    // A member of a call result is a member of the named type the resolved callee
    // declares as its return type, written as an annotation or, in JavaScript, in a
    // documentation comment.
    let source = "export class Item {
  value = 1
  read() { return this.value }
  chain(): Item { return this }
}
export function create(): Item { return new Item() }
export function maybe(): Item | undefined { return undefined }
export function loose() { return new Item() }
export function generic<T>(value: T): T { return value }
export function use() {
  return [create().read(), create().value, create().chain().read(), maybe().read()]
}
export function refuse() { return [loose().read(), generic(new Item()).read(), missing().read()] }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for to in ["main.ts/Item/read", "main.ts/Item/chain"] {
        assert!(has_edge(&index, EdgeKind::Calls, "main.ts/use", to), "{to}");
    }
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/use",
        "main.ts/Item/value"
    ));
    // No declared return type, a return type written as a type parameter and an
    // unresolved callee.
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && edge.from().as_str().ends_with("main.ts/refuse")
            && edge.to().as_str().ends_with("main.ts/Item/read")
    }));
    let script = "class Help { visible() { return [] } }
class Command {
  /** @returns {Help} */
  createHelp() { return new Help() }
  info() { return this.createHelp().visible() }
}
exports.Command = Command;
";
    let (_directory, index) = workspace(&[("command.js", script)]);
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "command.js/Command/info",
        "command.js/Help/visible"
    ));
}

#[test]
fn test_imports_of_a_directory_specifier_name_its_index_module() {
    // `'./'` and `'.'` name the directory of the importing file, and a directory names
    // its index module only.
    let (_directory, index) = workspace_with_manifest(
        &[
            (
                "stream.ts",
                "export const stream = (value: number) => value\n",
            ),
            ("index.ts", "export { stream } from './stream'\n"),
            (
                "text.ts",
                "import { stream } from './'\nimport { stream as again } from '.'\nimport { stream as gone } from './missing/'\nexport const text = (value: number) => [stream(value), again(value), gone(value)]\n",
            ),
        ],
        module_manifest(),
    );
    assert_eq!(call_count(&index, "text.ts/text", "stream.ts/stream"), 2);
    assert!(has_edge(&index, EdgeKind::Imports, "text.ts", "index.ts"));
}

#[test]
fn test_typescript_private_fields_keep_their_declared_types() {
    // A private field's written type types its value as a public field's does.
    let source = "export class Node { insert() { return 1 } }
export class Trie {
  #root: Node = new Node()
  plain: Node = new Node()
  #loose = 1
  insert() { return [this.#root.insert(), this.plain.insert(), this.#loose] }
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert_eq!(
        call_count(&index, "main.ts/Trie/insert", "main.ts/Node/insert"),
        2
    );
}

#[test]
fn test_members_of_a_constructed_value_belong_to_its_class() {
    // `new Item().read()` reads a member of the class `new` constructs, as a binding
    // initialized by that `new` does.
    let source = "export class Item { value = 1; read() { return this.value } }
export function make() { return 1 }
export function use() { return [new Item().read(), new Item().value] }
export function refuse() { return [new make().read(), new Missing().read()] }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/use",
        "main.ts/Item/read"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/use",
        "main.ts/Item/value"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && edge.from().as_str().ends_with("main.ts/refuse")
            && edge.to().as_str().ends_with("/read")
    }));
}

#[test]
fn test_calls_through_bindings_given_only_functions_target_the_binding() {
    // A binding assigned again keeps its calls when every value it is given is a function
    // written in place; the call targets the binding declaration. A value that may not be
    // callable keeps the refusal.
    let utils = "export let isOld = (): boolean => {
  const result = true
  isOld = () => result
  return result
}
export function make() { return 1 }
export let mixed = () => 1
mixed = make
export let later = function () { return 1 }
later = function () { return 2 }
";
    let main = "import { isOld, mixed, later } from './utils'
export function use() { return [isOld(), later()] }
export function refuse() { return mixed() }
";
    let (_directory, index) =
        workspace_with_manifest(&[("utils.ts", utils), ("main.ts", main)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/use",
        "utils.ts/isOld"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/use",
        "utils.ts/later"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("main.ts/refuse")
    }));
}

#[test]
fn test_array_method_callbacks_take_the_declared_element_type() {
    // A callback of a standard array method takes the element type the receiver's
    // declared array type writes, at the method's element positions.
    let source = "export class Option { name() { return 'o' } }
export class Command {
  options: Option[] = []
  list(): Array<Option> { return this.options }
  each() { this.options.forEach((option) => option.name()) }
  mapped() { return this.list().map((option) => option.name()) }
  filtered(extra: readonly Option[]) { return extra.filter((option) => option.name()) }
  found(maybe: Option[] | undefined) { return maybe.find((option) => option.name()) }
  reduced() { return this.options.reduce((total, option) => option.name(), '') }
  sorted() { return this.options.sort((left, right) => left.name().length - right.name().length) }
  loose(values: any[]) { return values.forEach((value) => value.name()) }
  accumulator(values: Option[]) { return values.reduce((total, value) => total.name(), 0) }
  shadow(options: Option[]) {
    const inner = (options) => options.forEach((option) => option.name())
    return inner
  }
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for caller in ["each", "mapped", "filtered", "found", "reduced", "sorted"] {
        assert!(
            has_edge(
                &index,
                EdgeKind::Calls,
                &format!("main.ts/Command/{caller}"),
                "main.ts/Option/name"
            ),
            "{caller}"
        );
    }
    // An element type the receiver does not declare, the accumulator position and a
    // parameter that shadows the declared array type nothing.
    for caller in ["loose", "accumulator", "shadow"] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Calls
                    && edge
                        .from()
                        .as_str()
                        .contains(&format!("main.ts/Command/{caller}"))
                    && edge.to().as_str().ends_with("main.ts/Option/name")
            }),
            "{caller}"
        );
    }
    let script = "class Option { name() { return 'o' } }
class Command {
  constructor() {
    /** @type {Option[]} */
    this.options = [];
    this.plain = [];
  }
  /** @returns {Command[]} */
  ancestors() { return [this] }
  /** @param {Option[]} list */
  each(list) { list.forEach((option) => option.name()) }
  fields() { this.options.forEach((option) => option.name()) }
  walk() { this.ancestors().forEach((cmd) => cmd.fields()) }
  folded() { return this.ancestors().reduce((all, cmd) => cmd.fields(), {}) }
  loose() { this.plain.forEach((option) => option.name()) }
}
exports.Command = Command;
";
    let (_directory, index) = workspace(&[("command.js", script)]);
    for (from, to) in [
        ("command.js/Command/each", "command.js/Option/name"),
        ("command.js/Command/fields", "command.js/Option/name"),
        ("command.js/Command/walk", "command.js/Command/fields"),
        ("command.js/Command/folded", "command.js/Command/fields"),
    ] {
        assert!(
            has_edge(&index, EdgeKind::Calls, from, to),
            "{from} -> {to}"
        );
    }
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "command.js/Command/loose",
        "command.js/Option/name"
    ));
}

#[test]
fn test_commonjs_members_of_a_module_value_assigned_by_name() {
    // A member read through a whole-module `require` of `module.exports = name`, with
    // `name` an object declared in that module, is a member of that object. A module that
    // assigns its value twice names no member.
    let defaults = "function getAdapter() { return 1 }
var defaults = { adapter: getAdapter(), transform: function transform() { return 2 } };
module.exports = defaults;
";
    let twice = "var first = { adapter: 1 };
var second = { adapter: 2 };
module.exports = first;
module.exports = second;
";
    let main = "var defaults = require('./defaults');
var twice = require('./twice');
function use() { return [defaults.adapter, defaults.transform()] }
function refuse() { return twice.adapter }
exports.use = use;
exports.refuse = refuse;
";
    let (_directory, index) = workspace(&[
        ("defaults.js", defaults),
        ("twice.js", twice),
        ("main.js", main),
    ]);
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.js/use",
        "defaults.js/defaults/adapter"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "defaults.js/defaults/transform"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Reads
            && edge.from().as_str().ends_with("main.js/refuse")
            && edge.to().as_str().ends_with("/adapter")
    }));
}

#[test]
fn test_calls_through_shorthand_members_of_an_exported_object() {
    // `api.leaf()` with `export const api = { leaf }` calls the member `leaf`, which holds
    // the imported function; a member holding a value that is not callable stays uncalled.
    let leaf = "export function leaf() { return 1 }
export const value = 1
";
    let api = "import { leaf, value } from './leaf'
export const api = { leaf, value }
";
    let probe = "import { api } from './api'
export function use() { return api.leaf() }
export function refuse() { return api.value() }
";
    let (_directory, index) = workspace_with_manifest(
        &[("leaf.ts", leaf), ("api.ts", api), ("probe.ts", probe)],
        module_manifest(),
    );
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "probe.ts/use",
        "api.ts/api/leaf"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("probe.ts/refuse")
    }));
}

#[test]
fn test_this_inside_object_literal_methods_names_the_literal_members() {
    // Outside every class, `this` in a method of an object literal is that literal: its
    // members resolve there. An arrow function keeps the surrounding `this`, a nested
    // `function` binds its own, and a local of a method is no member.
    let source = "module.exports = (function cookies() {
  return {
    write: function write(name) { return name },
    read(name) { return this.write(name) },
    remove: function remove(name) { this.write(name) },
    nested: function nested() {
      return function inner() { return this.write(1) }
    },
    shadow() {
      const helper = 1
      return this.helper
    },
  }
})();
";
    let (_directory, index) = workspace(&[("main.js", source)]);
    let calls = calls(&index);
    for caller in ["read", "remove"] {
        assert!(
            calls
                .iter()
                .any(|(from, to)| from.ends_with(caller) && to.ends_with("write")),
            "{caller}: {calls:?}"
        );
    }
    assert!(
        !calls.iter().any(|(from, _)| from.ends_with("inner")),
        "{calls:?}"
    );
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Reads && edge.to().as_str().ends_with("/shadow/helper")
    }));
}

#[test]
fn test_typescript_callbacks_take_the_written_type_of_an_assigned_member() {
    let source = r"
export interface Options { storage: string }
export interface Input<T> { get: () => T }
type AddSettings<T> = T extends { get: () => infer U }
    ? { settings: { apply: (options: Partial<Options>) => string | undefined } }
    : never;
type Other = { settings: { apply: (options: { storage: number }) => number } };
type Alternatives<T> = T extends { get: () => infer U }
    ? { settings: { apply: (options: Options) => string } }
    : Other;

export function install(api: unknown) {
    (api as Input<string> & AddSettings<Input<string>>).settings = {
        apply: (options) => options.storage,
    };
}
export function conflicting(api: unknown) {
    (api as Other & AddSettings<Input<string>>).settings = {
        apply: (options) => options.storage,
    };
}
export function alternatives(api: unknown) {
    (api as Input<string> & Alternatives<Input<string>>).settings = {
        apply: (options) => options.storage,
    };
}
export function explicitly_typed(api: unknown) {
    (api as AddSettings<Input<string>>).settings = {
        apply: (options: { storage: string }) => options.storage,
    };
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    let reads = index
        .edges()
        .edges()
        .filter(|edge| {
            edge.kind() == EdgeKind::Reads
                && edge.to().as_str().ends_with("main.ts/Options/storage")
        })
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 1, "{reads:?}");
    let from = reads[0].from().as_str();
    assert!(from.ends_with("main.ts/install/apply"), "{from}");
}

#[test]
fn test_typescript_callbacks_of_asserted_inline_object_types() {
    // A callback passed to a member of a value asserted to an inline object type takes
    // the parameter types the asserted member's function type writes.
    let source = "export interface Message { type: string; payload: unknown }
export function listen(connection: unknown) {
  ;(connection as unknown as {
    subscribe: (listener: (message: Message) => void) => void
  }).subscribe((message) => { return message.type })
}
export function own(connection: unknown) {
  ;(connection as { subscribe: (listener: (message: Message) => void) => void }).subscribe(
    (message: { type: number }) => message.type,
  )
}
export function loose(connection: unknown) {
  ;(connection as { subscribe: (listener: (message: any) => void) => void }).subscribe(
    (message) => message.type,
  )
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/listen",
        "main.ts/Message/type"
    ));
    // A parameter with its own annotation and a position without a named type.
    for caller in ["main.ts/own", "main.ts/loose"] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Reads
                    && edge.from().as_str().ends_with(caller)
                    && edge.to().as_str().ends_with("main.ts/Message/type")
            }),
            "{caller}"
        );
    }
}

#[test]
fn test_javascript_bound_function_owns_direct_prototype_methods() {
    let source = "var Writer = function() {};
Writer.prototype.write = function() {};
var Alias = Writer;
Alias.prototype.hidden = function() {};
var Replaced = function() {};
Replaced.prototype = {};
Replaced.prototype.hidden = function() {};
var Rebound = function() {};
Rebound.prototype.hidden = function() {};
Rebound = function() {};
const Arrow = () => {};
Arrow.prototype.hidden = function() {};
const Async = async function() {};
Async.prototype.hidden = function() {};
const Generator = function*() {};
Generator.prototype.hidden = function() {};
Early.prototype.hidden = function() {};
var Early = function() {};
";
    let (_directory, index) = workspace(&[("writer.js", source)]);
    let view = index.symbol_view().expect("symbol view");
    let graph = view.normalized_graph();
    let identity = |suffix: &str| {
        graph.records().iter().find(|record| {
            record
                .identity()
                .is_some_and(|name| name.as_str().ends_with(suffix))
        })
    };
    for (name, kind) in [
        ("writer.js/Writer", "variable"),
        ("writer.js/Writer/write", "method"),
    ] {
        let symbol = identity(name).expect("authored symbol");
        assert!(symbol.contributions().iter().any(|key| {
            graph
                .contribution(key)
                .and_then(rift_core::Contribution::facts)
                .is_some_and(|fact| fact.kind().0 == kind)
        }));
    }
    for owner in [
        "Alias",
        "Replaced",
        "Rebound",
        "Arrow",
        "Async",
        "Generator",
        "Early",
    ] {
        assert!(
            identity(&format!("writer.js/{owner}/hidden")).is_none(),
            "{owner}"
        );
    }
    let mut edges = index.edges().edges().filter(|edge| {
        edge.kind() == EdgeKind::Contains
            && edge.from().as_str().ends_with("writer.js/Writer")
            && edge.to().as_str().ends_with("writer.js/Writer/write")
    });
    let edge = edges.next().expect("stored containment");
    assert!(edges.next().is_none());
    assert_eq!(edge.occurrence().unit().key().as_str(), "writer.js");
    let range = edge.occurrence().range();
    let start = usize::try_from(range.start()).expect("method start");
    let end = usize::try_from(range.end()).expect("method end");
    assert_eq!(
        start,
        source.find("prototype.write").expect("method") + "prototype.".len()
    );
    assert_eq!(&source[start..end], "write");
}

#[test]
fn test_this_assignments_declare_members_and_join_existing_facets() {
    // `this.name = value` in a constructor or method declares member `name` of the class;
    // a same-named field is another facet of that member. Members of an object literal
    // assigned that way are members of the member. A same-named local stays a local.
    let source = "function leaf() { return 1 }
export class Item {
  run!: () => number
  constructor() {
    this.run = () => leaf()
    this.options = { verbose: true, write: function write() { return 2 } }
  }
  reset() { this.count = 0; const count = 1; return count }
}
export function use(item: Item) { return [item.run(), item.options.write(), item.count] }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/use",
        "main.ts/Item/run"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/Item/run",
        "main.ts/leaf"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/use",
        "main.ts/Item/options/write"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/use",
        "main.ts/Item/count"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Contains,
        "main.ts/Item",
        "main.ts/Item/options"
    ));
    // A subclass assigning an inherited member through `this` overrides nothing.
    let derived = "export class Base { router: number = 1 }
export class Derived extends Base { constructor() { super(); this.router = 2 } }
";
    let (_directory, derived) = workspace_with_manifest(&[("main.ts", derived)], module_manifest());
    assert!(
        !derived
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Overrides)
    );
    // The local `count` is no member.
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Reads
            && edge.from().as_str().ends_with("main.ts/use")
            && edge.to().as_str().ends_with("main.ts/Item/reset/count")
    }));
    let script = "function Manager() {}
function Axios(config) {
  this.defaults = config;
  this.interceptors = { request: new Manager(), response: new Manager() };
}
Axios.prototype.request = function request(config) { return this.defaults };
module.exports = Axios;
";
    let (_directory, index) = workspace(&[("Axios.js", script)]);
    let view = index.symbol_view().expect("view");
    let identities = view
        .normalized_graph()
        .records()
        .iter()
        .filter_map(|record| {
            record
                .identity()
                .map(|identity| identity.as_str().to_owned())
        })
        .collect::<Vec<_>>();
    for name in [
        "Axios.js/Axios/request",
        "Axios.js/Axios/interceptors",
        "Axios.js/Axios/interceptors/request",
        "Axios.js/Axios/defaults",
    ] {
        assert!(
            identities.iter().any(|identity| identity.ends_with(name)),
            "{name}: {identities:?}"
        );
    }
    let language =
        rift_protocol::read::Language::from_identity_segment("javascript").expect("language");
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("identity coverage")
    );
}

#[test]
fn test_bindings_from_calls_take_members_every_returned_literal_writes() {
    // A callee without a declared return type that returns an object literal from every
    // `return` gives a binding of its call result the members every literal writes.
    let source = "function split(flags) {
  let shortFlag;
  let longFlag;
  if (!flags) throw new Error('none');
  return { shortFlag, longFlag };
}
function pick(value) {
  if (value) return { left: 1, both: 2 };
  return { right: 1, both: 3 };
}
function mixed(value) {
  if (value) return { kept: 1 };
  return value;
}
function use(flags) {
  const parts = split(flags);
  const picked = pick(flags);
  const other = mixed(flags);
  return [parts.shortFlag, parts.longFlag, picked.both, picked.left, other.kept];
}
exports.use = use;
";
    let (_directory, index) = workspace(&[("main.js", source)]);
    for to in [
        "main.js/split/shortFlag",
        "main.js/split/longFlag",
        "main.js/pick/both",
    ] {
        assert!(has_edge(&index, EdgeKind::Reads, "main.js/use", to), "{to}");
    }
    // A member missing in one return, and a return that is no object literal.
    for to in ["main.js/pick/left", "main.js/mixed/kept"] {
        assert!(
            !has_edge(&index, EdgeKind::Reads, "main.js/use", to),
            "{to}"
        );
    }
}

#[test]
fn test_calls_through_typed_fields_written_again_keep_the_field_type() {
    // A field with a written type holds a value of that type whatever writes it, so a call
    // through it keeps the type's member. An untyped field written twice gives no target.
    let source = "export interface Usage {
  fail(msg: string): void
  help(): string
}
export function usage(): Usage {
  return { fail(msg: string) {}, help() { return '' } }
}
export class Instance {
  #usage: Usage
  helper: Usage
  constructor() {
    this.#usage = this!.#usage
    this.helper = usage()
  }
  reset() {
    this.#usage = usage()
    this.helper = usage()
  }
  check(): string {
    this.#usage.fail('x')
    return this.helper.help()
  }
}
class Right { run() { return 2 } }
export class Untyped {
  constructor() { this.side = { run: () => 3 } }
  swap() { this.side = new Right() }
  use() { return this.side.run() }
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/Instance/check",
        "main.ts/Usage/fail"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/Instance/check",
        "main.ts/Usage/help"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("main.ts/Untyped/use")
    }));
}

#[test]
fn test_written_callable_fields_keep_their_call_and_read_after_assignment() {
    let source = "export type Handler = (value: string) => number
export class Owner {
  readonly named: Handler
  inline: (value: string) => number
  constructor(handle: Handler) { this.named = handle; this.inline = handle }
  invoke() { return this.named('named') + this.inline('inline') }
}
export class Patched {
  leaf() { return 1 }
  use() { return this.leaf() }
  patch(value: () => number) { this.leaf = value }
}
export class Inferred {
  handle = (value: string) => value.length
  use() { return this.handle('inferred') }
  patch(handle: Handler) { this.handle = handle }
}
export class Unknown {
  handle: unknown
  constructor(handle: unknown) { this.handle = handle }
  use() {
    if (typeof this.handle === 'function') return this.handle('unknown')
  }
}
export class Replaced {
  handle: Handler
  constructor(handle: Handler) { this.handle = handle }
  use() { return this.handle('written') }
}
export function replace(target: Replaced, handle: Handler) { target.handle = handle }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for field in ["named", "inline"] {
        let target = format!("main.ts/Owner/{field}");
        let callee = format!("this.{field}");
        let call = format!("{callee}('{field}')");
        for (kind, expected) in [(EdgeKind::Calls, &call), (EdgeKind::Reads, &callee)] {
            let mut edges = index.edges().edges().filter(|edge| {
                edge.kind() == kind
                    && edge.from().as_str().ends_with("main.ts/Owner/invoke")
                    && edge.to().as_str().ends_with(&target)
            });
            let edge = edges.next().unwrap_or_else(|| panic!("{kind:?} {target}"));
            let range = edge.occurrence().range();
            let start = usize::try_from(range.start()).expect("start");
            let end = usize::try_from(range.end()).expect("end");
            assert_eq!(&source[start..end], expected);
            assert!(edges.next().is_none());
        }
    }
    for name in ["Patched", "Inferred", "Unknown", "Replaced"] {
        let owner = format!("main.ts/{name}/use");
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with(&owner)
            }),
            "{owner}"
        );
    }
}

const CALLABLE_FIELD_COPY_SOURCE: &str = "
export type Handler = (value: string) => string

export class DirectCopy {
  handle: Handler = value => value
  clone() {
    const clone = new DirectCopy()
    clone.handle = this.handle
    return clone
  }
  invoke() { return this.handle('direct') }
}

export class ContinuedExpression {
  handle: Handler = value => value
  clone() {
    const clone = new ContinuedExpression()
    clone.handle = this.handle
      .bind(this)
    return clone
  }
  invoke() { return this.handle('continued') }
}

export class Parenthesized {
  handle: Handler = value => value
  clone() {
    const clone = new Parenthesized()
    clone.handle = (this.handle)
    return clone
  }
  invoke() { return this.handle('wrapped') }
}

export class OtherField {
  handle: Handler = value => value
  other: Handler = value => value.toUpperCase()
  clone() {
    const clone = new OtherField()
    clone.handle = this.other
    return clone
  }
  invoke() { return this.handle('different') }
}

export class Rebound {
  handle: Handler = value => value
  clone(replacement: Rebound) {
    let clone = new Rebound()
    clone = replacement
    clone.handle = this.handle
    return clone
  }
  invoke() { return this.handle('rebound') }
}

export class Method {
  handle(value: string): string { return value }
  clone() {
    const clone = new Method()
    clone.handle = this.handle
    return clone
  }
  invoke() { return this.handle('method') }
}

export class Unknown {
  handle: unknown
  clone() {
    const clone = new Unknown()
    clone.handle = this.handle
    return clone
  }
  invoke() {
    if (typeof this.handle === 'function') return this.handle('unknown')
  }
}
";

#[test]
fn test_written_callable_field_copy_needs_the_complete_value_and_const_instance() {
    let source = CALLABLE_FIELD_COPY_SOURCE;
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    for owner in [
        "DirectCopy",
        "ContinuedExpression",
        "Parenthesized",
        "OtherField",
        "Rebound",
        "Method",
        "Unknown",
    ] {
        let caller = format!("main.ts/{owner}/invoke");
        let field = format!("main.ts/{owner}/handle");
        let occurrences = |kind| {
            index
                .edges()
                .edges()
                .filter(|edge| {
                    edge.kind() == kind
                        && edge.from().as_str().ends_with(&caller)
                        && edge.to().as_str().ends_with(&field)
                })
                .map(|edge| {
                    let range = edge.occurrence().range();
                    let start = usize::try_from(range.start()).expect("start");
                    let end = usize::try_from(range.end()).expect("end");
                    &source[start..end]
                })
                .collect::<Vec<_>>()
        };
        assert!(
            occurrences(EdgeKind::Reads).contains(&"this.handle"),
            "{owner} read"
        );
        let expected: &[_] = if owner == "DirectCopy" {
            &["this.handle('direct')"]
        } else {
            &[]
        };
        assert_eq!(occurrences(EdgeKind::Calls), expected, "{owner} call");
    }
}

#[test]
fn test_type_assertions_on_initializers_type_their_locals() {
    // `const name = value as T` types `name` as `T`, as `const name: T = value` does; the
    // last assertion of a chain decides. A union or an assertion to no named type types
    // nothing.
    let source = "export interface Handle { setBlocking(on: boolean): void }
export interface Stream { handle: Handle; isTTY: boolean }
export function single(value: unknown) {
  const stream = value as Stream
  stream.handle.setBlocking(true)
  return stream.isTTY
}
export function double(value: unknown) {
  const stream = value as any as Stream
  return stream.isTTY
}
export function inCallback(values: unknown[]) {
  return values.map(function each(item) {
    const stream = item as Stream
    return stream.isTTY
  })
}
export function wide(value: unknown) {
  const either = value as Stream | Handle
  const last = value as Stream as unknown
  return [either.isTTY, last.isTTY]
}
export interface Usage { fail(msg: string): void; help(): string }
export function asserted(): Usage {
  const self = {} as Usage
  self.fail('x')
  return self
}
export function annotated(): Usage {
  const self: Usage = {} as Usage
  self.fail('x')
  return self
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    let edges = index
        .edges()
        .edges()
        .map(|edge| {
            format!(
                "{:?} {} {}",
                edge.kind(),
                edge.from().as_str(),
                edge.to().as_str()
            )
        })
        .collect::<Vec<_>>();
    for (kind, from, to) in [
        (EdgeKind::Reads, "main.ts/single", "main.ts/Stream/isTTY"),
        (
            EdgeKind::Calls,
            "main.ts/single",
            "main.ts/Handle/setBlocking",
        ),
        (EdgeKind::Reads, "main.ts/double", "main.ts/Stream/isTTY"),
        (
            EdgeKind::Reads,
            "main.ts/inCallback",
            "main.ts/Stream/isTTY",
        ),
        (EdgeKind::Calls, "main.ts/asserted", "main.ts/Usage/fail"),
        (EdgeKind::Calls, "main.ts/annotated", "main.ts/Usage/fail"),
    ] {
        assert!(has_edge(&index, kind, from, to), "{from} {to}: {edges:#?}");
    }
    assert!(
        !edges
            .iter()
            .any(|edge| edge.contains("main.ts/wide ") && edge.ends_with("/isTTY")),
        "{edges:#?}"
    );
}

#[test]
fn test_names_destructured_from_require_read_members_of_the_module_value() {
    // `const { name } = require(module)` binds what `require(module).name` reads: with
    // `module.exports = value` and `value` an object declared in that module, the member
    // of that object. A key the object does not write binds nothing.
    let symbols = "const keys = {
  kState: Symbol('state'),
  make: function make() { return 1 }
}
module.exports = keys
";
    let main = "const { kState, make, kMissing } = require('./symbols.js')
function use(server) { return [server[kState], make(), kMissing] }
module.exports = use
";
    let (_directory, index) = workspace(&[("symbols.js", symbols), ("main.js", main)]);
    for (name, target) in [
        ("main.js/kState", "symbols.js/keys/kState"),
        ("main.js/make", "symbols.js/keys/make"),
    ] {
        assert!(has_edge(&index, EdgeKind::Imports, name, target), "{name}");
    }
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.js/use",
        "symbols.js/keys/make"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Imports && edge.from().as_str().ends_with("main.js/kMissing")
    }));
}

#[test]
fn test_commonjs_members_added_after_the_export_value() {
    // `module.exports = value` followed by `module.exports.name = ...` adds `name` to
    // that value. A replacement written later drops the name, and so does `exports.name`
    // after the replacement, since `exports` no longer names the exported object.
    let reply = "function Reply() {}
function buildReply(R) { return R }
function early() { return 1 }
function extra() { return 2 }
exports.early = early
module.exports = Reply
module.exports.buildReply = buildReply
exports.extra = extra
";
    let main = "const Reply = require('./reply.js')
function build() {
  return [new Reply(), Reply.buildReply(Reply), Reply.early(), Reply.extra()]
}
module.exports = build
";
    let (_directory, index) = workspace(&[("reply.js", reply), ("main.js", main)]);
    for to in ["reply.js/Reply", "reply.js/buildReply"] {
        assert!(
            has_edge(&index, EdgeKind::Calls, "main.js/build", to),
            "{to}"
        );
    }
    for to in ["reply.js/early", "reply.js/extra"] {
        assert!(
            !has_edge(&index, EdgeKind::Calls, "main.js/build", to),
            "{to}"
        );
    }
}

#[test]
fn test_members_typed_by_the_global_function_type_are_callable() {
    // A member written with the global `Function` type is callable through a field or a
    // parameter of its owner; it constructs nothing, and a workspace `Function`
    // declaration without a call signature decides instead.
    let source = "export interface Shim {
  cliui: Function
  readdirSync: (path: string) => string[]
}
export class Command {
  #shim: Shim
  constructor(shim: Shim) { this.#shim = shim }
  add(dir: string) {
    this.#shim.readdirSync(dir)
    return this.#shim.cliui()
  }
}
export function viaParameter(shim: Shim) { return shim.cliui() }
export function constructs(shim: Shim) { return new shim.cliui() }
";
    let shadowed = "interface Function { name: string }
export interface Local { run: Function }
export function local(item: Local) { return item.run() }
";
    let (_directory, index) = workspace_with_manifest(
        &[("main.ts", source), ("shadowed.ts", shadowed)],
        module_manifest(),
    );
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/Command/add",
        "main.ts/Shim/cliui"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/Command/add",
        "main.ts/Shim/readdirSync"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/viaParameter",
        "main.ts/Shim/cliui"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/constructs",
        "main.ts/Shim/cliui"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "shadowed.ts/local",
        "shadowed.ts/Local/run"
    ));
}

#[test]
fn test_typescript_types_named_like_a_function_take_the_interface() {
    // In TypeScript a function and an interface of one name merge: a type written with that
    // name is the interface, also inside the function itself.
    let source = "export interface Box<T> { value: T }
export function Box<T>(value: T): Box<T> { return { value } }
export function make(): Box<number> { return Box(1) }
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::HasType,
        "main.ts/make",
        "main.ts/Box"
    ));
    assert!(index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::HasType
            && edge.from().as_str().ends_with("main.ts/Box")
            && edge.to().as_str().ends_with("main.ts/Box")
    }));
}

#[test]
fn test_typescript_utility_projections_read_as_their_type() {
    // `Partial<T>`, `Readonly<T>` and `Required<T>` keep every member name of `T`, so
    // members read through them are the members of `T`. No other utility type does.
    let source = "export interface Options { name: string; run(): number }
export function projected(p: Partial<Options>, r: Readonly<Options>, q: Required<Options> | undefined) {
  return [p.name, r.run(), q?.name]
}
export function picked(k: Pick<Options, 'name'>, n: NonNullable<Options>) {
  return [k.name, n.name]
}
";
    let (_directory, index) = workspace_with_manifest(&[("main.ts", source)], module_manifest());
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/projected",
        "main.ts/Options/name"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "main.ts/projected",
        "main.ts/Options/run"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/picked",
        "main.ts/Options/name"
    ));
}

#[test]
fn test_object_literals_ending_in_a_typed_spread_take_its_members() {
    // An object literal whose last element spreads a name of type `T` has the members `T`
    // declares from `T`. In TypeScript a `let` keeps the type of its initializer through
    // later writes; in JavaScript a binding written again keeps nothing.
    let source = "export interface Options { name: string; version?: number; run?: () => number }
export function spreadLast(base: Options) {
  const o = { version: 0, run: () => 1, ...base }
  return [o.name, o.version, o.run?.()]
}
export function writtenAgain(base: Options, next: Partial<Options>) {
  let o = { version: 0, ...base }
  o = { ...o, ...next }
  return o.name
}
export function spreadFirst(base: Options) {
  const o = { ...base, version: 1 }
  return o.name
}
type Impl = <T>(init: () => T, options: Options) => () => void
export const impl: Impl = (init, baseOptions) => () => {
  let options = { version: 0, run: () => 2, ...baseOptions }
  const setItem = () => [options.name, options.version, options.run?.()]
  options = { ...options, name: 'x' }
  return setItem
}
";
    let script = "class Options { constructor() { this.name = 'a' } }
/** @param {Options} base */
function kept(base) {
  const o = { ...base }
  return o.name
}
/** @param {Options} base */
function written(base, other) {
  let o = { ...base }
  o = other
  return o.name
}
module.exports = { kept, written, Options }
";
    let (_directory, index) = workspace_with_manifest(
        &[("main.ts", source), ("script.cjs", script)],
        module_manifest(),
    );
    for (kind, from, to) in [
        (
            EdgeKind::Reads,
            "main.ts/spreadLast",
            "main.ts/Options/name",
        ),
        (
            EdgeKind::Reads,
            "main.ts/spreadLast",
            "main.ts/Options/version",
        ),
        (EdgeKind::Calls, "main.ts/spreadLast", "main.ts/Options/run"),
        (
            EdgeKind::Reads,
            "main.ts/writtenAgain",
            "main.ts/Options/name",
        ),
        (
            EdgeKind::Reads,
            "main.ts/impl/setItem",
            "main.ts/Options/name",
        ),
        (
            EdgeKind::Calls,
            "main.ts/impl/setItem",
            "main.ts/Options/run",
        ),
    ] {
        assert!(has_edge(&index, kind, from, to), "{from} {to}");
    }
    assert!(has_edge(
        &index,
        EdgeKind::Reads,
        "script.cjs/kept",
        "script.cjs/Options/name"
    ));
    // A spread followed by other members, and a JavaScript binding written again.
    assert!(!has_edge(
        &index,
        EdgeKind::Reads,
        "main.ts/spreadFirst",
        "main.ts/Options/name"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Reads
            && edge.from().as_str().ends_with("script.cjs/written")
            && edge.to().as_str().ends_with("/name")
    }));
}

#[test]
fn test_commonjs_symbol_keyed_export_writes_name_no_export() {
    // A computed key on `module.exports` or `exports` names no export when it is a `const`
    // of the same file bound to a `Symbol(..)` value. Any other computed key still leaves
    // every export unresolved for calls.
    let parser = "const kInternals = Symbol('internals')
function run() { return 1 }
function Parser() {}
module.exports = Parser
module.exports.run = run
module.exports[kInternals] = { run }
exports[kInternals] = run
";
    let other = "const kName = 'run'
function run() { return 1 }
function Other() {}
module.exports = Other
module.exports.run = run
module.exports[kName] = run
";
    let main = "const Parser = require('./parser.js')
const Other = require('./other.js')
function build() {
  return [new Parser(), Parser.run(), new Other(), Other.run()]
}
module.exports = build
";
    let (_directory, index) = workspace(&[
        ("parser.js", parser),
        ("other.js", other),
        ("main.js", main),
    ]);
    for to in ["parser.js/Parser", "parser.js/run"] {
        assert!(
            has_edge(&index, EdgeKind::Calls, "main.js/build", to),
            "{to}"
        );
    }
    for to in ["other.js/Other", "other.js/run"] {
        assert!(
            !has_edge(&index, EdgeKind::Calls, "main.js/build", to),
            "{to}"
        );
    }
}

#[test]
fn test_this_writes_store_the_member_they_resolve_to() {
    // `this.name = value` always stores a write from the code that runs it. An ancestor
    // the static pass resolves that declares `name` is the target; otherwise the member
    // the assignment declares is, also under an ancestor that does not resolve.
    let entry = "import * as core from './index.js';
import { External } from 'missing-package';
interface Value {}
class Derived extends core.Base<Value> { run(input: Value): Value { this.value = core.value; return this.value; } }
class Own { run() { this.count = 1; return this.count } }
class Outside extends External { run() { this.count = 1; return this.count } }
export { Derived, Own, Outside }
";
    let (_directory, index) = workspace_with_manifest(
        &[
            ("entry.ts", entry),
            ("index.ts", "export * from './base.js';"),
            (
                "base.ts",
                "export const value = 1;\nexport class Base<T> { value: T; run(input: T): T { return input; } }",
            ),
        ],
        module_manifest(),
    );
    for (kind, from, to) in [
        (
            EdgeKind::Writes,
            "entry.ts/Derived/run",
            "base.ts/Base/value",
        ),
        (
            EdgeKind::Reads,
            "entry.ts/Derived/run",
            "base.ts/Base/value",
        ),
        (EdgeKind::Reads, "entry.ts/Derived/run", "base.ts/value"),
        (EdgeKind::Writes, "entry.ts/Own/run", "entry.ts/Own/count"),
        (EdgeKind::Reads, "entry.ts/Own/run", "entry.ts/Own/count"),
        (
            EdgeKind::Writes,
            "entry.ts/Outside/run",
            "entry.ts/Outside/count",
        ),
    ] {
        assert!(has_edge(&index, kind, from, to), "{kind:?} {from} {to}");
    }
    // No write is owned by the member it writes.
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| { edge.kind() == EdgeKind::Writes && edge.from() == edge.to() })
    );
}

/// The entry a workspace package names holds its package identity, whatever form its
/// manifest writes; any other file keeps the identity of its own path.
#[test]
fn test_npm_entries_follow_extensions_folders_and_index_files() {
    let load = "exports.load = function () {};\n";
    let types = "export declare function load(): void;\n";
    for (metadata, files, entry) in [
        (r#","main":"index.js""#, vec!["index.js"], Some("index.js")),
        (
            r#","type":"commonjs","main":"index.js""#,
            vec!["index.js"],
            Some("index.js"),
        ),
        (
            r#","main":"./index.cjs""#,
            vec!["index.cjs"],
            Some("index.cjs"),
        ),
        (r#","main":"./index""#, vec!["index.js"], Some("index.js")),
        (r#","main":"index""#, vec!["index.js"], Some("index.js")),
        (r#","main":".""#, vec!["index.js"], Some("index.js")),
        ("", vec!["index.js"], Some("index.js")),
        (
            r#","main":"lib""#,
            vec!["index.js", "lib/index.js"],
            Some("lib/index.js"),
        ),
        (
            r#","main":"./lib/""#,
            vec!["index.js", "lib/index.js"],
            Some("lib/index.js"),
        ),
        (r#","main":"./gone""#, vec!["index.js"], Some("index.js")),
        (
            r#","main":"./data.json""#,
            vec!["index.js", "data.json"],
            None,
        ),
        (
            r#","types":"./index""#,
            vec!["index.d.ts"],
            Some("index.d.ts"),
        ),
        ("", vec!["index.d.ts"], Some("index.d.ts")),
        (r#","main":"../index""#, vec!["index.js"], None),
        (r#","main":"lib""#, vec!["lib/other.js"], None),
    ] {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(
            directory.path().join("package.json"),
            format!(r#"{{"name":"beacon","version":"1.0.0"{metadata}}}"#),
        )
        .expect("manifest");
        std::fs::create_dir(directory.path().join("lib")).expect("folder");
        for path in &files {
            let text = match path.rsplit_once('.') {
                Some((_, "ts")) => types,
                Some((_, "json")) => "{}",
                _ => load,
            };
            std::fs::write(directory.path().join(path), text).expect("source");
        }
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("index");
        let view = index.symbol_view().expect("view");
        let placed = view
            .normalized_graph()
            .records()
            .iter()
            .filter(|record| {
                record
                    .identity()
                    .is_some_and(|identity| identity.as_str().ends_with("/beacon/load"))
            })
            .count();
        assert_eq!(placed, usize::from(entry.is_some()), "{metadata} {files:?}");
        let own = entry.map(|entry| format!("%2F{}/load", entry.replace('/', "%2F")));
        assert!(
            !view.normalized_graph().records().iter().any(|record| {
                record.identity().is_some_and(|identity| {
                    own.as_ref()
                        .is_some_and(|own| identity.as_str().ends_with(own.as_str()))
                })
            }),
            "{metadata} the entry has no second identity"
        );
    }
}

/// A file the index leaves out is no missing file: the written entry stays the entry, and the
/// package's own index takes no identity from it.
#[test]
fn test_npm_entries_outside_the_index_leave_the_package_index_its_own_path() {
    let directory = tempfile::tempdir().expect("workspace");
    let write = |path: &str, text: &str| {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("folder")).expect("folder");
        std::fs::write(path, text).expect("file");
    };
    std::fs::create_dir(directory.path().join(".git")).expect("repository");
    write(".gitignore", "dist/\n");
    write("index.js", "exports.load = function () {};\n");
    write("dist/a.js", "exports.load = function () {};\n");
    let large = format!("exports.load = function () {{}};\n//{}\n", "x".repeat(4096));
    write("big.js", &large);
    for (main, limited) in [("dist/a.js", false), ("./dist/a", false), ("./big", true)] {
        write(
            "package.json",
            &format!(r#"{{"name":"beacon","version":"1.0.0","main":"{main}"}}"#),
        );
        let mut limits = WorkspaceIndexLimits::default();
        if limited {
            let syntax = limits.syntax();
            limits = limits.with_syntax(
                rift_syntax::SyntaxLimits::new(
                    2048,
                    syntax.syntax_elements_max(),
                    syntax.syntax_depth_max(),
                )
                .expect("syntax limits"),
            );
        }
        let index = WorkspaceIndex::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("index");
        let view = index.symbol_view().expect("view");
        let identities = view
            .normalized_graph()
            .records()
            .iter()
            .filter_map(|record| record.identity())
            .map(|identity| identity.as_str().to_owned())
            .filter(|identity| identity.ends_with("/load"))
            .collect::<Vec<_>>();
        let expected = ["big.js", "index.js"]
            .into_iter()
            .filter(|path| !limited || *path == "index.js")
            .map(|path| format!("rift://symbol/local/javascript/.%2F{path}/load"))
            .collect::<Vec<_>>();
        assert_eq!(identities, expected, "{main}");
    }
}

#[test]
fn test_commonjs_members_of_exported_literals_and_assigned_objects() {
    // A `name: value` pair of an exported literal that no later property replaces names its
    // value wherever it is written; an object assigned to a member after the export holds
    // its own members.
    let literal = "module.exports = { kDirect: Symbol('direct'), exist: checkExistence, last: 1 }
function checkExistence () {}
";
    let parser = "function Parser () {}
module.exports = Parser
module.exports.helpers = { add: function add () {}, size: 1 }
";
    let replaced = "function Replaced () {}
function other () {}
module.exports = Replaced
module.exports.helpers = { add: function add () {} }
module.exports.helpers.add = other
module.exports.tools = { run: function run () {} }
module.exports.tools = other
module.exports.computed = { run: function run () {} }
module.exports.computed[other] = other
";
    let twice = "module.exports = { kTwice: Symbol('twice'), exist: checkTwice }
function checkTwice () {}
module.exports = { kTwice: 1 }
";
    let written = "module.exports = { kWritten: Symbol('written'), exist: checkWritten }
function checkWritten () {}
module.exports.kWritten = 1
module.exports.exist = 1
";
    let main = "const { kDirect, exist } = require('./literal')
const Parser = require('./parser')
const Replaced = require('./replaced')
const { kTwice, exist: existTwice } = require('./twice')
const { kWritten, exist: existWritten } = require('./written')
function use (server) { return [server[kDirect], exist(), Parser.helpers.add()] }
function refuse (server) {
  return [Replaced.helpers.add(), Replaced.tools.run(), Replaced.computed.run(), server[kTwice], existTwice(), server[kWritten], existWritten()]
}
module.exports = { use, refuse }
";
    let (_directory, index) = workspace(&[
        ("literal.js", literal),
        ("parser.js", parser),
        ("replaced.js", replaced),
        ("twice.js", twice),
        ("written.js", written),
        ("main.js", main),
    ]);
    for (kind, from, to) in [
        (
            EdgeKind::Imports,
            "main.js/kDirect",
            "literal.js/default/kDirect",
        ),
        (
            EdgeKind::Imports,
            "main.js/exist",
            "literal.js/default/exist",
        ),
        (EdgeKind::Calls, "main.js/use", "literal.js/checkExistence"),
        (EdgeKind::Calls, "main.js/use", "parser.js/helpers/add"),
        (EdgeKind::Reads, "main.js/use", "parser.js/helpers"),
        (EdgeKind::Reads, "main.js/use", "parser.js/helpers/add"),
        (
            EdgeKind::Contains,
            "parser.js/helpers",
            "parser.js/helpers/add",
        ),
    ] {
        assert!(has_edge(&index, kind, from, to), "{kind:?} {from} {to}");
    }
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("main.js/refuse")
    }));
    // A literal replaced later supplies nothing. A property written again is no longer the
    // literal's: the import names the assignment that declares the export's value.
    for name in ["kTwice", "existTwice", "kWritten", "existWritten"] {
        assert!(
            !index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Imports
                    && edge.from().as_str().ends_with(&format!("main.js/{name}"))
                    && edge.to().as_str().contains("/default/")
            }),
            "{name}"
        );
    }
    assert!(has_edge(
        &index,
        EdgeKind::Imports,
        "main.js/kWritten",
        "written.js/kWritten"
    ));
    let javascript =
        rift_protocol::read::Language::from_identity_segment("javascript").expect("language");
    assert!(
        index
            .identity_coverage_complete(&javascript)
            .expect("identity coverage")
    );
}

/// The three files of express beside TypeScript's `CommonJS` output: a synchronized function
/// export, named exports of values and required names, and a module value the file names by
/// an alias.
const COMMONJS_MODULES: &[(&str, &str)] = &[
    (
        "application.js",
        "var app = exports = module.exports = {};
app.init = function init() { return helper() };
app.handle = handleRequest;
function handleRequest() {}
function helper() {}
",
    ),
    (
        "dynamic.js",
        "var app = exports = module.exports = {};
app.init = function init() {};
app.handle = handleRequest;
app[name] = function () {};
function handleRequest() {}
",
    ),
    (
        "utils.js",
        "exports.methods = ['get'];
exports.etag = createETag({ weak: false });
exports.normalize = function normalize() {};
exports.Client = class Client { connect() {} static make() {} };
var taken = createETag;
exports.taken = createETag({ weak: true });
function createETag() {}
",
    ),
    (
        "express.js",
        "var proto = require('./application');
var dynamic = require('./dynamic');
var utils = require('./utils');
exports = module.exports = createApplication;
function createApplication() {
  proto.init();
  proto.handle();
  dynamic.init();
  dynamic.handle();
  utils.normalize();
  return [utils.methods, utils.etag, utils.taken, new utils.Client(), utils.Client.make()];
}
exports.application = proto;
",
    ),
    (
        "output.js",
        "Object.defineProperty(exports, '__esModule', { value: true });
const util_1 = require('./utils');
Object.defineProperty(exports, 'normalize', { enumerable: true, get: function () { return util_1.normalize; } });
__exportStar(require('./application2'), exports);
",
    ),
    (
        "application2.js",
        "function run() {}
exports.run = run;
exports.default = run;
",
    ),
    (
        "main.js",
        "const { normalize, run } = require('./output');
const output = require('./output');
function use() { return [normalize(), run(), output.run(), output.normalize()] }
function refuse() { return [output.default(), output()] }
module.exports = { use, refuse };
",
    ),
];

#[test]
fn test_commonjs_module_value_aliases_and_assigned_values_resolve() {
    let (_directory, index) = workspace(COMMONJS_MODULES);
    for (kind, from, to) in [
        (
            EdgeKind::Calls,
            "express.js/createApplication",
            "application.js/app/init",
        ),
        (
            EdgeKind::Calls,
            "express.js/createApplication",
            "application.js/handleRequest",
        ),
        (
            EdgeKind::Calls,
            "express.js/createApplication",
            "utils.js/normalize",
        ),
        (
            EdgeKind::Calls,
            "express.js/createApplication",
            "utils.js/Client",
        ),
        (
            EdgeKind::Calls,
            "express.js/createApplication",
            "utils.js/Client/make",
        ),
        (
            EdgeKind::Reads,
            "express.js/createApplication",
            "utils.js/methods",
        ),
        (
            EdgeKind::Reads,
            "express.js/createApplication",
            "utils.js/etag",
        ),
        (
            EdgeKind::Reads,
            "express.js/createApplication",
            "utils.js/exports/taken",
        ),
        (EdgeKind::Writes, "%2Futils.js", "utils.js/methods"),
        (EdgeKind::Calls, "main.js/use", "utils.js/normalize"),
        (EdgeKind::Calls, "main.js/use", "application2.js/run"),
        (EdgeKind::Imports, "main.js/normalize", "utils.js/normalize"),
        (
            EdgeKind::Contains,
            "utils.js/Client",
            "utils.js/Client/connect",
        ),
    ] {
        assert!(has_edge(&index, kind, from, to), "{kind:?} {from} {to}");
    }
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && (edge.to().as_str().contains("dynamic.js")
                || edge.from().as_str().ends_with("main.js/refuse"))
    }));
    // No write is owned by the declaration it writes.
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Writes
            && edge.from() == edge.to()
            && edge.to().as_str().contains("utils.js")
    }));
    let javascript =
        rift_protocol::read::Language::from_identity_segment("javascript").expect("language");
    assert!(
        index
            .identity_coverage_complete(&javascript)
            .expect("identity coverage")
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_commonjs_export_keys_imported_as_symbols_name_no_export() {
    // A computed key on `module.exports` whose value is a symbol declared in another module
    // names no export. A string, a rebound name, an unresolved import and a name a later
    // write changes leave every export unresolved for calls.
    let keys = "const kInternals = Symbol('internals')
const kShared = Symbol.for('shared')
const kName = 'run'
let kLoose = Symbol('loose')
module.exports = { kInternals, kShared, kName, kLoose }
";
    let bag = "const keys = { kState: Symbol('state'), kText: 'run' }
module.exports = keys
";
    let literal = "module.exports = { kPair: Symbol('pair'), kOther: 'run' }
";
    let changed = "const kChanged = Symbol('changed')
exports.kChanged = kChanged
exports.kChanged = 'run'
";
    let reexport = "const { kInternals } = require('./keys.js')
module.exports.kInternals = kInternals
";
    let owner = |import: &str, key: &str| {
        format!(
            "{import}
function run() {{ return 1 }}
function Owner() {{}}
module.exports = Owner
module.exports.run = run
module.exports[{key}] = {{ run }}
"
        )
    };
    let accepted = [
        ("const { kInternals } = require('./keys.js')", "kInternals"),
        ("const { kShared } = require('./keys.js')", "kShared"),
        ("const { kState } = require('./bag.js')", "kState"),
        ("const { kPair } = require('./literal.js')", "kPair"),
        (
            "const { kInternals: kKey } = require('./reexport.js')",
            "kKey",
        ),
        ("const kOwn = Symbol.for('own')", "kOwn"),
    ];
    let refused = [
        ("const { kName } = require('./keys.js')", "kName"),
        ("const { kLoose } = require('./keys.js')", "kLoose"),
        ("const { kText } = require('./bag.js')", "kText"),
        ("const { kOther } = require('./literal.js')", "kOther"),
        ("const { kChanged } = require('./changed.js')", "kChanged"),
        ("const { kMissing } = require('./missing.js')", "kMissing"),
        ("const { kExternal } = require('external')", "kExternal"),
        (
            "let { kInternals } = require('./keys.js'); kInternals = 'run'",
            "kInternals",
        ),
        ("const Symbol = String; const kOwn = Symbol('run')", "kOwn"),
    ];
    let mut files = vec![
        ("keys.js".to_owned(), keys.to_owned()),
        ("bag.js".to_owned(), bag.to_owned()),
        ("literal.js".to_owned(), literal.to_owned()),
        ("changed.js".to_owned(), changed.to_owned()),
        ("reexport.js".to_owned(), reexport.to_owned()),
    ];
    let owners = accepted.iter().chain(&refused).enumerate();
    let mut requires = Vec::new();
    let mut calls = Vec::new();
    for (index, (import, key)) in owners {
        files.push((format!("owner{index}.js"), owner(import, key)));
        requires.push(format!("const Owner{index} = require('./owner{index}.js')"));
        calls.push(format!("new Owner{index}(), Owner{index}.run()"));
    }
    let main = format!(
        "{}\nfunction build() {{ return [{}] }}\nmodule.exports = build\n",
        requires.join("\n"),
        calls.join(", ")
    );
    files.push(("main.js".to_owned(), main));
    let files = files
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let (_directory, index) = workspace(&files);
    for (position, (import, _)) in accepted.iter().chain(&refused).enumerate() {
        for member in ["Owner", "run"] {
            assert_eq!(
                has_edge(
                    &index,
                    EdgeKind::Calls,
                    "main.js/build",
                    &format!("owner{position}.js/{member}")
                ),
                position < accepted.len(),
                "{import} {member}"
            );
        }
    }
}
