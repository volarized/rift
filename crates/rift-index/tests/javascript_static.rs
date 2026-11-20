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
