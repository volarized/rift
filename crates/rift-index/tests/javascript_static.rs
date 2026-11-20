//! Stored JavaScript and TypeScript edges from captured source.

use rift_core::{SourceVisibility, TextFileInclusion};
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

