#[test]
fn workspace_javascript_initializer_calls_keep_their_execution_owners() {
    let source = concat!(
        "function load() {}\n",
        "export const direct = load();\n",
        "export const object = { property: load() };\n",
        "load();\n",
        "export class Holder { field = load(); }\n",
        "export const later = () => load();\n",
    );
    for (language, path) in [("javascript", "main.mjs"), ("typescript", "main.ts")] {
        let result = resolution(language, &[(path, source)], &[], WORK_MAX);
        let mut found = calls(&result, path).collect::<Vec<_>>();
        found.sort_unstable_by_key(|edge| edge.binding.range().start());
        assert_eq!(found.len(), 5, "{language}: {found:?}");
        let sites = source.match_indices("load()").skip(1);
        for ((edge, (start, _)), suffix) in
            found
                .into_iter()
                .zip(sites)
                .zip(["", "", "", "/Holder/field", "/later"])
        {
            assert_eq!(
                edge.binding.range().start(),
                u64::try_from(start).expect("offset")
            );
            assert_eq!(source_of(edge, source), "load()");
            assert!(target(edge).ends_with("/load"), "{language}: {edge:?}");
            let owner = edge.edge_from.as_ref().expect("caller");
            assert!(
                owner.as_str().ends_with(&format!(".%2F{path}{suffix}")),
                "{language}: {owner}"
            );
        }
    }
}

#[test]
fn test_typescript_export_overloads_keep_one_checked_callable() {
    let source = "function leaf() {} export function run(value: string): void; export function run(value: number): void; export function run(value: unknown) { leaf(); }";
    let document = syntax("jobs.ts", source);
    assert_eq!(
        document
            .facts()
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == "run")
            .count(),
        3
    );
    let result = resolution(
        "typescript",
        &[
            (
                "main.ts",
                "import { run } from './jobs.js'; export function root() { run(1); }",
            ),
            ("jobs.ts", source),
        ],
        &[],
        WORK_MAX,
    );
    let imported = calls(&result, "main.ts").collect::<Vec<_>>();
    assert_eq!(imported.len(), 1, "{result:?}");
    assert!(target(imported[0]).ends_with(".%2Fjobs.ts/run"));
    assert!(
        imported[0]
            .edge_from
            .as_ref()
            .expect("caller")
            .as_str()
            .ends_with(".%2Fmain.ts/root")
    );
    assert_eq!(calls(&result, "jobs.ts").count(), 1);
}

#[test]
fn test_ecmascript_exported_duplicate_bodies_stay_ambiguous() {
    for (language, main, jobs) in [
        ("javascript", "main.mjs", "jobs.mjs"),
        ("typescript", "main.ts", "jobs.ts"),
    ] {
        let source =
            format!("import {{ run }} from './{jobs}'; export function root() {{ run(); }}");
        let result = resolution(
            language,
            &[
                (main, &source),
                (jobs, "export function run() {} export function run() {}"),
            ],
            &[],
            WORK_MAX,
        );
        assert_eq!(calls(&result, main).count(), 0, "{language}: {result:?}");
        if language == "typescript" {
            let result = resolution(
                language,
                &[
                    (main, &source),
                    (
                        jobs,
                        "export function run() {} export function run(value: string): void; export function run() {}",
                    ),
                ],
                &[],
                WORK_MAX,
            );
            assert_eq!(calls(&result, main).count(), 0, "{result:?}");
        }
    }
}

#[test]
fn test_commonjs_local_object_members_resolve_only_stable_callables() {
    for (change, expected) in [
        ("", 2),
        ("object.run = other;", 1),
        ("object[key] = other;", 1),
        ("object = other;", 1),
        ("[object] = values;", 1),
        ("ob\\u006aect = other;", 1),
        ("object.r\\u0075n = other;", 1),
        ("object.run = function() {};", 1),
        ("function change(object) { object.run = other; }", 2),
        ("other.run = unknown;", 2),
    ] {
        for initializer in ["{}", "Object.create(null)"] {
            let source = format!(
                "function leaf() {{}} var object = {initializer}; object /* assignment */ . run = function() {{ leaf(); }}; {change} object /* call */ . run();"
            );
            for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
                let result = resolution(language, &[(path, &source)], &[], WORK_MAX);
                assert_eq!(
                    calls(&result, path).count(),
                    expected,
                    "{language}: {source}: {result:?}"
                );
            }
        }
    }
}

#[test]
fn test_commonjs_local_object_members_resolve_only_their_receiver_binding() {
    for source in [
        "var res = {}; res.clear = function clear() { const opts = { expires: 1 }; return opts.expires; }; res.set = function() { const opts = {}; opts.expires = 2; return opts; }; module.exports = res;",
        "function root(receiver) { receiver.clear = function() { const opts = { expires: 1 }; return opts.expires; }; receiver.set = function() { const opts = {}; opts.expires = 2; }; }",
    ] {
        for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
            let result = resolution(language, &[(path, source)], &[], WORK_MAX);
            let accesses = result
                .occurrences
                .iter()
                .filter(|edge| {
                    matches!(edge.role, ReferenceRole::Read | ReferenceRole::Write)
                        && target(edge).ends_with("/expires")
                })
                .collect::<Vec<_>>();
            assert_eq!(accesses.len(), 1, "{language}: {source}: {accesses:?}");
            assert_eq!(accesses[0].role, ReferenceRole::Read);
            assert_eq!(source_of(accesses[0], source), "opts.expires");
        }
    }
}

#[test]
fn test_commonjs_prototype_methods_supply_callers_and_containment() {
    let source = "function leaf() {} function View() {} View.prototype.render = function render() { leaf(); render(); }; function Other() {} Other.prototype.lookup = function() { leaf(); };";
    for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
        let result = resolution(language, &[(path, source)], &[], WORK_MAX);
        let body_calls = calls(&result, path).collect::<Vec<_>>();
        assert_eq!(body_calls.len(), 3, "{language}: {result:?}");
        assert!(
            body_calls[0]
                .edge_from
                .as_ref()
                .expect("method caller")
                .as_str()
                .ends_with("/View/render")
        );
        assert!(target(body_calls[1]).ends_with("/View/render"));
        assert!(
            body_calls[2]
                .edge_from
                .as_ref()
                .expect("method caller")
                .as_str()
                .ends_with("/Other/lookup")
        );
        let contains = result
            .occurrences
            .iter()
            .filter(|edge| {
                edge.edge_kind == Some(rift_protocol::read::EdgeKind::Contains)
                    && target(edge).ends_with("/View/render")
            })
            .collect::<Vec<_>>();
        assert_eq!(contains.len(), 1);
        assert!(
            contains[0]
                .edge_from
                .as_ref()
                .expect("constructor owner")
                .as_str()
                .ends_with("/View")
        );
    }
}

#[test]
fn test_commonjs_module_require_imports_keep_file_and_lexical_binding_facts() {
    let source = "var other = require('./other.cjs'); var path = require('node:path'); function use() { return other; } function usePath() { return path; } module.exports = other;";
    for (language, path, other) in [
        ("javascript", "main.cjs", "other.cjs"),
        ("typescript", "main.cts", "other.cts"),
    ] {
        let document = syntax(path, source);
        let facts = document.facts().static_facts().expect("static facts");
        let binding = facts
            .bindings()
            .iter()
            .find(|binding| {
                &source[usize::try_from(binding.name.start).expect("start")
                    ..usize::try_from(binding.name.end).expect("end")]
                    == "other"
            })
            .expect("local binding");
        assert!(matches!(
            binding.kind,
            rift_syntax::StaticBindingKind::JavaScriptRequire { export: None, .. }
        ));
        let result = resolution(
            language,
            &[
                (path, source),
                (other, "var app = { value: 1 }; module.exports = app;"),
            ],
            &[],
            WORK_MAX,
        );
        let imports = result
            .occurrences
            .iter()
            .filter(|edge| {
                edge.role == ReferenceRole::Import && edge.binding.unit().key().as_str() == path
            })
            .collect::<Vec<_>>();
        assert_eq!(imports.len(), 1, "{language}: {imports:?}");
        assert!(imports[0].target_binding.is_none(), "file import");
        assert!(target(imports[0]).contains("other."));
        assert!(
            result
                .occurrences
                .iter()
                .any(|edge| edge.role == ReferenceRole::Read && target(edge).ends_with("/path")),
            "{language}: local external require binding"
        );
        assert_eq!(
            result
                .occurrences
                .iter()
                .filter(|edge| edge.role == ReferenceRole::Read && target(edge).ends_with("/other"))
                .count(),
            2,
            "{language}"
        );
    }
}

#[test]
fn test_commonjs_module_require_imports_obey_package_default_and_module_syntax() {
    let require = "var other = require('./other.js'); function use() { return other; }";
    let metadata = [("package.json", r#"{"name":"fixture","version":"1.0.0"}"#)];
    for (prefix, expected) in [
        ("", 1),
        ("async function nested() { await task; }", 1),
        ("const nested = async () => { await task; };", 1),
        ("{ let module; }", 1),
        ("var module;", 1),
        ("void import('./side.js');", 1),
        ("export {};", 0),
        ("import './side.js';", 0),
        ("const url = import.meta.url;", 0),
        ("await task;", 0),
        ("for await (const item of items) {}", 0),
        ("const module = {};", 0),
        ("let __filename;", 0),
        ("class __dirname {}", 0),
        ("const { module } = wrapper;", 0),
    ] {
        let source = format!("{prefix}\n{require}");
        let result = resolution(
            "javascript",
            &[("main.js", &source), ("other.js", "module.exports = {};")],
            &metadata,
            WORK_MAX,
        );
        assert_eq!(
            result
                .occurrences
                .iter()
                .filter(|edge| edge.role == ReferenceRole::Import)
                .count(),
            expected,
            "{prefix}: {result:?}"
        );
    }
    for prefix in [
        "function use(require) {",
        "function use(require, require) {",
        "function use() { const require = loader;",
        "function use() { require = loader;",
    ] {
        let source = format!("{prefix} var other = require('./other.js'); return other; }}");
        let result = resolution(
            "javascript",
            &[("main.js", &source), ("other.js", "module.exports = {};")],
            &metadata,
            WORK_MAX,
        );
        assert!(
            !result
                .occurrences
                .iter()
                .any(|edge| edge.role == ReferenceRole::Import),
            "{prefix}"
        );
    }
}

#[test]
fn test_commonjs_module_require_imports_refuse_ambiguous_file_candidates() {
    for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
        let result = resolution(
            language,
            &[
                (
                    path,
                    "var other = require('./other'); function use() { return other; }",
                ),
                ("other.js", "module.exports = {};"),
                ("other.ts", "module.exports = {};"),
            ],
            &[],
            WORK_MAX,
        );
        assert!(
            !result
                .occurrences
                .iter()
                .any(|edge| edge.role == ReferenceRole::Import)
        );
    }
}

#[test]
fn test_commonjs_function_assignments_resolve_export_and_local_names() {
    let source = "function leaf() {}\nexports.run = function(value) { leaf(); };\nexports.other = function inner() { inner(); leaf(); };\nmodule.exports.third = function named() { leaf(); };\nfunction hidden(exports) { exports.run(); }\ninner();";
    for (language, path, other) in [
        ("javascript", "main.cjs", "other.cjs"),
        ("typescript", "main.cts", "other.cts"),
    ] {
        let result = resolution(
            language,
            &[
                (path, source),
                (other, "module.exports = function(value) { return 1; };"),
            ],
            &[],
            WORK_MAX,
        );
        let pairs = calls(&result, path)
            .map(|call| {
                (
                    call.edge_from
                        .as_ref()
                        .map(|id| id.as_str().rsplit('/').next().expect("caller")),
                    target(call).rsplit('/').next().expect("callee"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            [
                (Some("run"), "leaf"),
                (Some("other"), "other"),
                (Some("other"), "leaf"),
                (Some("third"), "leaf")
            ],
            "{language}"
        );
    }
}

#[test]
fn test_commonjs_function_assignments_refuse_duplicate_and_shadowed_exports() {
    let source = "exports.run = function first() {};\nexports.run = function second() {};\nexports.guard = function() {};\nexports.run(); exports.guard();\nfunction shadow(exports) { exports.guard(); }";
    for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
        let result = resolution(language, &[(path, source)], &[], WORK_MAX);
        let targets = calls(&result, path).map(target).collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{language}: {targets:?}");
        assert!(targets[0].ends_with("/guard"));
    }
}

#[test]
fn test_commonjs_function_assignments_resolve_anonymous_module_values() {
    for (language, path, other) in [
        ("javascript", "main.cjs", "other.cjs"),
        ("typescript", "main.cts", "other.cts"),
    ] {
        let result = resolution(
            language,
            &[
                (path, "const run = require('./other.cjs'); run();"),
                (other, "module.exports = function(value) { return value; };"),
            ],
            &[],
            WORK_MAX,
        );
        let targets = calls(&result, path).map(target).collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{language}: {targets:?}");
        assert!(targets[0].ends_with("/default"));
    }
}

#[test]
fn test_commonjs_function_assignments_do_not_name_inline_callbacks_as_outer_functions() {
    let source =
        "function leaf() {} function root() { unknown(function inner() { inner(); }); leaf(); }";
    for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
        let result = resolution(language, &[(path, source)], &[], WORK_MAX);
        let targets = calls(&result, path).map(target).collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{language}: {targets:?}");
        assert!(targets[0].ends_with("/leaf"));
    }
}

#[test]
fn test_commonjs_function_assignments_refuse_later_export_mutations() {
    for mutation in [
        "function change() { exports.run = replacement; }",
        "function change() { exports /* receiver */ . run = replacement; }",
        "function change(require) { exports.run = replacement; }",
        "function change() { module.exports.run = replacement; }",
        "function change() { module.exports = replacement; }",
        "function change() { exports[key] = replacement; }",
        "function change() { exports.run++; }",
        "function change() { exp\\u006frts.run = replacement; }",
        "function change() { exports.r\\u0075n = replacement; }",
    ] {
        let source = format!("exports.run = function() {{}}; {mutation} exports.run();");
        for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
            let result = resolution(language, &[(path, &source)], &[], WORK_MAX);
            assert_eq!(calls(&result, path).count(), 0, "{language}: {mutation}");
        }
    }
    for mutation in [
        "function change(exports) { exports.run = replacement; }",
        "function change(module) { module.exports.run = replacement; }",
        "function change() { other.run = replacement; }",
        "function change() { exports.run.extra = replacement; }",
    ] {
        let source = format!("exports.run = function() {{}}; {mutation} exports.run();");
        for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
            let result = resolution(language, &[(path, &source)], &[], WORK_MAX);
            assert_eq!(calls(&result, path).count(), 1, "{language}: {mutation}");
        }
    }
}

#[test]
fn test_ecmascript_member_comments_keep_authored_access_targets() {
    let source =
        "const options = { value: 1 }; function run() { return options /* receiver */ . value; }";
    for (language, path) in [("javascript", "main.cjs"), ("typescript", "main.cts")] {
        let result = resolution(language, &[(path, source)], &[], WORK_MAX);
        assert_eq!(
            result
                .occurrences
                .iter()
                .filter(|edge| edge.role == ReferenceRole::Read
                    && target(edge).ends_with("/options/value"))
                .count(),
            1,
            "{language}"
        );
    }
}

#[test]
fn test_commonjs_function_assignments_distinguish_module_values_from_default_properties() {
    let source = "const whole = require('./other.cjs'); const member = require('./other.cjs').default; whole(); member();";
    for (language, path, other) in [
        ("javascript", "main.cjs", "other.cjs"),
        ("typescript", "main.cts", "other.cts"),
    ] {
        let result = resolution(
            language,
            &[
                (path, source),
                (other, "exports.default = function inner() {};"),
            ],
            &[],
            WORK_MAX,
        );
        let calls = calls(&result, path)
            .map(|edge| source_of(edge, source))
            .collect::<Vec<_>>();
        assert_eq!(calls, ["member()"], "{language}");
    }
}

#[test]
fn test_ecmascript_parameters_shadow_only_their_bound_names() {
    let source = "function leaf() {}\nfunction run(value) { leaf(); }\nfunction shadow(leaf) { leaf(); }\nfunction pattern({ leaf: renamed }, [other], ...rest) { leaf(); renamed(); other(); rest(); }\nfunction duplicate() { function leaf() {} function leaf() {} leaf(); }";
    for (language, path) in [("javascript", "main.js"), ("typescript", "main.ts")] {
        let document = syntax(path, source);
        assert!(
            document
                .facts()
                .static_facts()
                .expect("static facts")
                .unsupported_bindings()
                .is_empty()
        );
        let result = resolution(language, &[(path, source)], &[], WORK_MAX);
        let resolved = calls(&result, path).collect::<Vec<_>>();
        assert_eq!(resolved.len(), 2, "{language}: {resolved:?}");
        for call in resolved {
            assert!(target(call).ends_with("/leaf"));
            assert_eq!(source_of(call, source), "leaf()");
        }
    }
}

#[test]
fn test_typescript_declared_interface_calls_survive_unrelated_member_writes() {
    let source = "interface Task { run(): void; } function root(input: Task, other) { other.run = () => {}; input.run(); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/typescript/.%2Fmain.ts/Task/run"]
    );
}

#[test]
fn test_typescript_overloaded_methods_keep_one_logical_call_target() {
    let source = "interface Task { run(value: string): void; run(value: number): void; } class Worker { run(value: string): void; run(value: number): void; run(value: string | number) {} } function root(input: Task) { input.run('a'); const worker = new Worker(); worker.run(1); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts").map(target).collect::<Vec<_>>(),
        [
            "rift://symbol/local/typescript/.%2Fmain.ts/Task/run",
            "rift://symbol/local/typescript/.%2Fmain.ts/Worker",
            "rift://symbol/local/typescript/.%2Fmain.ts/Worker/run"
        ]
    );
}

#[test]
fn test_typescript_duplicate_method_implementations_remain_unresolved() {
    let source = "class Task { run() {} run() {} } const input = new Task(); input.run();";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/typescript/.%2Fmain.ts/Task"]
    );
}

#[test]
fn test_typescript_deferred_calls_resolve_later_lexical_bindings() {
    let source = "namespace Tasks { export const root = () => leaf(); export const leaf = () => {}; } leaf(); const leaf = () => {};";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let resolved = calls(&result, "main.ts").map(target).collect::<Vec<_>>();
    assert_eq!(
        resolved,
        ["rift://symbol/local/typescript/.%2Fmain.ts/Tasks/leaf"]
    );
}

#[test]
fn test_typescript_deferred_callbacks_keep_immediate_shadowing() {
    let source = "function schedule(callback) {} function root() { schedule(() => leaf()); const leaf = () => {}; } function immediate() { leaf(); const leaf = () => {}; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let resolved = calls(&result, "main.ts")
        .filter(|call| target(call).ends_with("/leaf"))
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        resolved,
        ["rift://symbol/local/typescript/.%2Fmain.ts/root/leaf"]
    );
}

#[test]
fn test_typescript_forward_class_types_keep_runtime_ordering() {
    let source = "interface Earlier { item: Later; } new Later(); class Later {}";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Type && target(edge).ends_with("/Later"))
            .count(),
        1
    );
    assert_eq!(calls(&result, "main.ts").count(), 0);
}

#[test]
fn test_typescript_lexical_accesses_preserve_shadowing_and_assignment_roles() {
    let source = "let value = 0; function root() { value = 1; value += 2; value++; } function shadow(value) { value++; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let count = |role| {
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == role && target(edge).ends_with("/value"))
            .count()
    };
    assert_eq!(count(ReferenceRole::Read), 2);
    assert_eq!(count(ReferenceRole::Write), 3);
}

#[test]
fn test_typescript_import_and_export_names_are_not_value_accesses() {
    let source = "import { leaf } from './leaf.js'; export { leaf }; leaf();";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("leaf.ts", "export function leaf() {}"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Read && target(edge).ends_with("/leaf"))
            .count(),
        1
    );
}

#[test]
fn test_typescript_declared_receiver_types_resolve_imported_members() {
    let source = "import type { Box as Alias } from './types.js'; export function root(input: Alias<string>) { input.leaf(); return input.value; }";
    let types = "export interface Box<T> { leaf(): void; value: T; } export interface Other { leaf(): void; value: number; }";
    let result = resolution(
        "typescript",
        &[("main.ts", source), ("types.ts", types)],
        &[],
        WORK_MAX,
    );
    let resolved = calls(&result, "main.ts").map(target).collect::<Vec<_>>();
    assert_eq!(
        resolved,
        ["rift://symbol/local/typescript/.%2Ftypes.ts/Box/leaf"]
    );
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Read && target(edge).ends_with("/Box/value"))
            .count(),
        1
    );
}

use rift_protocol::{identity::SymbolOwner, read::Language};
use rift_syntax::{SyntaxDocument, SyntaxLimits, SyntaxSource, registry};

use std::fmt::Write as _;

use super::{ReferenceRole, StaticOccurrence, StaticResolution, StaticSource, resolve};
use crate::analyzer::namespace::{SelectedFile, prepare_context};
use crate::semantic::PlacedFacts;
use crate::{ExactPackageLimits, NamespaceInput, PackageSource, WorkspaceSemantics};
use rift_core::ProjectPath;
use rift_provider::NormalizedTarget;
use rift_syntax::DocumentPlacement;

const WORK_MAX: usize = 10_000;

mod python_aliases;
mod python_ancestry;
mod python_callees;
mod python_class_rebinding;
mod python_conditional_methods;
mod python_decorators;
mod python_elements;
mod python_external_bases;
mod python_imports;
mod python_joins;
mod python_names;
mod python_optional;
mod python_overloads;
mod python_property_writes;
mod python_scopes;

#[test]
fn test_python_property_returns_resolve_instance_members_and_aliases() {
    let source = "class Item:\n    def read(self): pass\nclass Owner:\n    @property\n    def item(self) -> Item: return Item()\n    def use(self): return self.item.read()\nclass Child(Owner):\n    def use(self): return self.item.read()\ndef typed(owner: Owner):\n    owner.item.read()\n    alias = owner.item\n    alias.read()\ndef constructed(): return Owner().item.read()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let reads = calls(&result, "main.py")
        .filter(|edge| target(edge) == "rift://symbol/local/python/main/Item/read")
        .map(|edge| {
            (
                source_of(edge, source),
                edge.edge_from.as_ref().expect("caller").as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reads,
        [
            (
                "self.item.read()",
                "rift://symbol/local/python/main/Owner/use"
            ),
            (
                "self.item.read()",
                "rift://symbol/local/python/main/Child/use"
            ),
            ("owner.item.read()", "rift://symbol/local/python/main/typed"),
            ("alias.read()", "rift://symbol/local/python/main/typed"),
            (
                "Owner().item.read()",
                "rift://symbol/local/python/main/constructed"
            ),
        ],
        "{result:?}"
    );
    assert!(
        result.occurrences.iter().any(|edge| {
            edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && source_of(edge, source) == "self.item"
                && target(edge) == "rift://symbol/local/python/main/Owner/item"
        }),
        "{result:?}"
    );
    assert!(
        result.occurrences.iter().any(|edge| {
            edge.role == ReferenceRole::Type
                && source_of(edge, source) == "Item"
                && target(edge) == "rift://symbol/local/python/main/Item"
        }),
        "{result:?}"
    );
    assert!(!result.exhausted);
}

#[test]
fn test_python_property_returns_refuse_shadowing_in_decorated_classes() {
    let source = "class Item:\n    def read(self): pass\n@decorate\nclass Owner:\n    @property\n    def item(self) -> Item: pass\n    from missing import item\ndef use(owner: Owner): return owner.item.read()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert!(
        calls(&result, "main.py")
            .all(|edge| target(edge) != "rift://symbol/local/python/main/Item/read"),
        "{result:?}"
    );
}

#[test]
fn test_python_property_returns_follow_imported_receiver_annotations() {
    let result = resolution(
        "python",
        &[
            (
                "models.py",
                "class Item:\n    def read(self): pass\nclass Owner:\n    @property\n    def item(self) -> Item: pass\n",
            ),
            (
                "main.py",
                "from models import Owner\ndef use(owner: Owner): return owner.item.read()\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/models/Item/read"],
        "{result:?}"
    );
}

#[test]
fn test_python_property_returns_refuse_descriptors_shadowing_and_ambiguity() {
    let prefix = "class Item:\n    def read(self): pass\nclass Other: pass\n";
    let cases = [
        "class Owner:\n    @property\n    def item(self) -> Item: pass\n    @classmethod\n    def use(cls): return cls.item.read()\ndef use():\n    Owner.item.read()\n    alias = Owner\n    alias.item.read()\n",
        "property = unknown\nclass Owner:\n    @property\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @other.property\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    @decorate\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item | Other: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item: pass\n    @property\n    def item(self) -> Other: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item: pass\ndef use(owner: Owner):\n    owner = unknown\n    return owner.item.read()\n",
        "def outer(property):\n    class Owner:\n        @property\n        def item(self) -> Item: pass\n    def use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item: pass\n    item = unknown\ndef use(owner: Owner): return owner.item.read()\n",
    ];
    for case in cases {
        let source = format!("{prefix}{case}");
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        assert!(
            calls(&result, "main.py")
                .all(|edge| { target(edge) != "rift://symbol/local/python/main/Item/read" }),
            "{source}\n{result:?}"
        );
        assert!(!result.exhausted);
    }
}

#[test]
fn test_python_wildcard_imports_keep_modules_without_resolving_exports() {
    let source = "from .leaf import *\ndef use(): return leaf()\n";
    let result = resolution(
        "python",
        &[
            ("pkg/__init__.py", source),
            ("pkg/leaf.py", "value = 1\ndef leaf(): pass\n"),
        ],
        &[],
        WORK_MAX,
    );
    let imports = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Import)
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2, "{result:?}");
    assert_eq!(source_of(imports[0], source), "*");
    assert_eq!(source_of(imports[1], source), ".leaf");
    for import in imports {
        assert_eq!(target(import), "rift://symbol/local/python/pkg/leaf");
        assert_eq!(
            import.edge_from.as_ref().map(rift_core::SymbolId::as_str),
            Some("rift://symbol/local/python/pkg")
        );
    }
    assert_eq!(calls(&result, "pkg/__init__.py").count(), 0);
}

#[test]
fn test_python_wildcard_imports_refuse_unknown_and_ambiguous_modules() {
    let result = resolution(
        "python",
        &[
            ("main.py", "from missing import *\nfrom pkg.leaf import *\n"),
            ("pkg/leaf.py", "value = 1\n"),
            ("pkg/leaf/__init__.py", "value = 2\n"),
        ],
        &[],
        WORK_MAX,
    );
    assert!(
        result
            .occurrences
            .iter()
            .all(|edge| edge.role != ReferenceRole::Import),
        "{result:?}"
    );
}

#[test]
fn test_typescript_named_value_imports_keep_import_edges() {
    let result = resolution(
        "typescript",
        &[
            ("main.ts", "import { value } from './core.js';"),
            ("core.ts", "export const value = 1;"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Import && target(edge).ends_with("/value"))
            .count(),
        1
    );
}

#[test]
fn test_typescript_type_usage_excludes_same_named_value_functions() {
    let source = "function bigint() {} enum Task { Value } interface Value { count: bigint; value: Task.Value; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Type && target(edge).ends_with("/bigint"))
            .count(),
        0
    );
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Type && target(edge).ends_with("/Task/Value"))
            .count(),
        1
    );
}

#[test]
fn test_typescript_base_members_exclude_locals_in_a_same_named_function() {
    let source = "interface Base {} const Base = () => { const run = () => {}; }; interface Derived extends Base { run(): void; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Overrides))
            .count(),
        0
    );
}

#[test]
fn test_typescript_static_blocks_and_nested_headers_keep_their_receivers() {
    let source = "class Base { static leaf() {} static make() { return Base; } static init() { class Nested extends this.make() {} } static { this.leaf(); } }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts")
            .map(|call| source_of(call, source))
            .collect::<Vec<_>>(),
        ["this.make()", "this.leaf()"]
    );
}

#[test]
fn test_typescript_callable_initializers_keep_the_outer_caller() {
    let source = "function leaf() {} function make() { return leaf; } export function root() { const run: () => void = make(); run(); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let calls = calls(&result, "main.ts").collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.edge_from.as_ref().is_some_and(
        |source| source.as_str() == "rift://symbol/local/typescript/.%2Fmain.ts/root"
    )));
}

#[test]
fn test_typescript_anonymous_functions_do_not_keep_class_receivers() {
    let source = "function factory(callback) {} class Base { leaf() {} run() { factory(function () { this.leaf(); }); const arrow = () => this.leaf(); } }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let calls = calls(&result, "main.ts")
        .filter(|call| target(call).ends_with("/leaf"))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].callee_range.start,
        u64::try_from(source.rfind("this.leaf").expect("arrow receiver")).expect("source offset")
    );
}

#[test]
fn test_typescript_parse_errors_keep_calls_with_partial_coverage() {
    let source = "export function root() { leaf(); const = ; } function leaf() {}";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(calls(&result, "main.ts").count(), 1);
    assert!(
        result
            .edge_calls
            .values()
            .any(|coverage| !coverage.unavailable && !coverage.unresolved.is_empty())
    );
}

#[test]
fn test_typescript_object_functions_do_not_keep_class_receivers() {
    let source = "class Base { leaf() {} run() { const handlers = { task() { this.leaf(); }, assigned: function () { this.leaf(); }, arrow: () => this.leaf() }; } }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let calls = calls(&result, "main.ts").collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].callee_range.start,
        u64::try_from(source.rfind("this.leaf").expect("arrow receiver")).expect("source offset")
    );
}

#[test]
fn test_typescript_cts_require_keeps_commonjs_calls() {
    let source = "const core = require('./core.cjs'); export function root() { core.leaf(); }";
    let result = resolution(
        "typescript",
        &[
            ("main.cts", source),
            ("core.cts", "exports.leaf = function () {};"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.cts").count(), 1);
}

#[test]
fn test_typescript_recursive_arrow_calls_keep_their_declaration() {
    let source = "const run = () => { run(); };";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(calls(&result, "main.ts").count(), 1);
}

#[test]
fn test_typescript_calls_use_the_enclosing_callable() {
    let source = "function leaf() {} export function root() { const value = leaf(); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let call = calls(&result, "main.ts").next().expect("call target");
    assert_eq!(
        call.edge_from.as_ref().map(rift_core::SymbolId::as_str),
        Some("rift://symbol/local/typescript/.%2Fmain.ts/root")
    );
}

#[test]
fn test_typescript_receiver_heritage_stays_unresolved_without_recursion() {
    let result = resolution(
        "typescript",
        &[("main.ts", "class Derived extends this.Base {}")],
        &[],
        256,
    );
    assert!(!result.exhausted);
    assert!(
        !result
            .occurrences
            .iter()
            .any(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Extends))
    );
}

#[test]
fn test_typescript_loop_bindings_shadow_outer_calls() {
    let source = "function run() {} export function root(values) { for (const [name, run] of values) { run(); } run(); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(calls(&result, "main.ts").count(), 1);
}

#[test]
fn test_typescript_computed_assignments_keep_unrelated_facts() {
    let source = "interface Input {} class Base { run() {} } class Derived extends Base implements Input { run() {} } export function leaf() {} export function root(values, key) { values[key] = 1; leaf(); } let replaced = () => 1; [replaced] = values; export function stop() { replaced(); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts")
            .map(|edge| source_of(edge, source))
            .collect::<Vec<_>>(),
        ["leaf()"]
    );
    for kind in [
        rift_protocol::read::EdgeKind::Extends,
        rift_protocol::read::EdgeKind::Implements,
        rift_protocol::read::EdgeKind::Overrides,
    ] {
        assert!(
            result
                .occurrences
                .iter()
                .any(|edge| edge.edge_kind == Some(kind)),
            "missing {kind:?}"
        );
    }
}

#[test]
fn test_typescript_super_constructor_and_parenthesized_values_resolve() {
    let source = "import { jobs } from './index.js'; class Base {} export class Derived extends Base { constructor() { super(); } } const run = (() => 1); export function root() { run(); jobs /* name */ . leaf(); }";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("index.ts", "export * as jobs from './jobs.js';"),
            ("jobs.ts", "export function leaf() {}"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(
        calls(&result, "main.ts")
            .map(|edge| source_of(edge, source))
            .collect::<Vec<_>>(),
        ["super()", "run()", "jobs /* name */ . leaf()"]
    );
}

#[test]
fn test_typescript_anonymous_default_functions_resolve_through_reexports() {
    let source = "import run from './index.js'; export function root() { run(); }";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("index.ts", "export { default } from './job.js';"),
            ("job.ts", "export default function () { return 1; }"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.ts").count(), 1);
}

#[test]
fn test_typescript_namespace_member_reads_name_the_module() {
    let source = "import * as core from './index.js'; export function root() { return core.jobs; }";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("index.ts", "export * as jobs from './jobs.js';"),
            ("jobs.ts", "export function leaf() {}"),
        ],
        &[],
        WORK_MAX,
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Read
                && target(edge) == "rift://symbol/local/typescript/.%2Fjobs.ts")
    );
}

#[test]
fn test_typescript_namespace_import_retains_an_empty_module() {
    let result = resolution(
        "typescript",
        &[
            (
                "main.ts",
                "import * as values from './empty.js'; export const value = 1;",
            ),
            ("empty.ts", ""),
        ],
        &[],
        WORK_MAX,
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Import
                && target(edge) == "rift://symbol/local/typescript/.%2Fempty.ts")
    );
}

#[test]
fn test_typescript_export_namespaces_and_default_objects_resolve_calls() {
    let source = "import * as core from './index.js'; import value from './object.js'; export function root() { core.jobs.run(); value.run(); }";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("index.ts", "export * as jobs from './jobs.js';"),
            (
                "jobs.ts",
                "export interface run {} export const run = () => 1;",
            ),
            ("object.ts", "export default { run: () => 1 };"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(
        calls(&result, "main.ts")
            .map(|edge| source_of(edge, source))
            .collect::<Vec<_>>(),
        ["core.jobs.run()", "value.run()"]
    );
}

#[test]
fn test_typescript_duplicate_exports_do_not_select_a_call_target() {
    let result = resolution(
        "typescript",
        &[
            (
                "main.ts",
                "import { run } from './index.js'; export function root() { run(); }",
            ),
            (
                "index.ts",
                "export { first as run, second as run } from './jobs.js';",
            ),
            (
                "jobs.ts",
                "export function first() {} export function second() {}",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.ts").count(), 0);
}

#[test]
fn test_typescript_static_calls_resolve_bases_constructors_and_export_stars() {
    // #761: all targets are declared in the selected source.
    let source = "import * as core from './index.js';\nimport Default from './base.js';\nclass Derived extends Default { run() { this.leaf(); super.leaf(); Derived.make(); } }\nexport function root() { core.leaf(); new Default(); new core.Base(); }";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("index.ts", "export * from './base.js';"),
            (
                "base.ts",
                "export function leaf() {}\nexport default class Base { leaf() {} static make() {} }\nexport { Base };",
            ),
        ],
        &[],
        WORK_MAX,
    );
    let actual = calls(&result, "main.ts")
        .map(|edge| source_of(edge, source))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            "this.leaf()",
            "super.leaf()",
            "Derived.make()",
            "core.leaf()",
            "new Default()",
            "new core.Base()"
        ],
        "{result:?}"
    );
}

#[test]
fn test_typescript_value_members_do_not_override_type_members() {
    let source = "interface Base { value: string; run(): void } interface Derived extends Base {} const Derived = { value: 'a', run() {} };";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert!(
        !result
            .occurrences
            .iter()
            .any(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Overrides))
    );
}

#[test]
fn test_typescript_overrides_keep_type_and_value_declarations_separate() {
    let source = "interface Base { readonly value: string; run(): void } const Base = 1; interface Derived extends Base { readonly value: string; run(): void } const Derived = 1;";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Overrides))
            .count(),
        2
    );
}

#[test]
fn test_typescript_static_members_keep_access_type_and_override_edges() {
    use rift_protocol::read::EdgeKind;

    // #762-#765: authored containment, access, override, and type facts.
    let source = "import * as config from './config.js';\ninterface Value {}\ninterface Task<T> {}\nclass Base { value: Value; run(input: Task<Value>): Value { return this.value; } }\nclass Derived extends Base { run(input: Task<Value>): Value { this.value = config.value; this.value++; return this.value; } }";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("config.ts", "export const value = 1;"),
        ],
        &[],
        WORK_MAX,
    );
    for kind in [
        EdgeKind::Contains,
        EdgeKind::Reads,
        EdgeKind::Writes,
        EdgeKind::Overrides,
        EdgeKind::HasType,
        EdgeKind::Imports,
    ] {
        assert!(
            result
                .occurrences
                .iter()
                .any(|edge| edge.edge_kind == Some(kind)
                    || match kind {
                        EdgeKind::Reads => edge.role == ReferenceRole::Read,
                        EdgeKind::Writes => edge.role == ReferenceRole::Write,
                        EdgeKind::HasType =>
                            edge.role == ReferenceRole::Type && edge.edge_kind.is_none(),
                        EdgeKind::Imports => edge.role == ReferenceRole::Import,
                        _ => false,
                    }),
            "missing {kind:?}: {result:?}"
        );
    }
}

#[test]
fn test_typescript_generic_class_and_interface_heritage_resolves_in_each_dialect() {
    use rift_protocol::read::EdgeKind;

    // #754 and #755: class type arguments and interface headers keep their bases.
    let source = "export abstract class Base<T> {}\ninterface Value {}\ninterface Task<T> extends Value {}\nclass Derived extends Base<Value> implements Task<Value> {}";
    for extension in ["ts", "tsx", "mts", "cts"] {
        let path = format!("main.{extension}");
        let result = resolution("typescript", &[(path.as_str(), source)], &[], WORK_MAX);
        let edges = result
            .occurrences
            .iter()
            .filter_map(|edge| {
                let kind = edge.edge_kind?;
                matches!(kind, EdgeKind::Extends | EdgeKind::Implements)
                    .then_some((kind, target(edge).rsplit('/').next()?))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            edges,
            [
                (EdgeKind::Extends, "Value"),
                (EdgeKind::Extends, "Base"),
                (EdgeKind::Implements, "Task")
            ],
            "{extension}: {result:?}"
        );
    }
}

#[test]
fn test_edge_heritage_is_captured_for_each_language() {
    for (path, source, expected) in [
        ("app.js", "class Base {}\nclass Derived extends Base {}", 1),
        (
            "app.ts",
            "class Base {}\ninterface Task {}\nclass Derived extends Base implements Task {}",
            2,
        ),
        (
            "app.tsx",
            "class Base {}\ninterface Task {}\nclass Derived extends Base implements Task {}",
            2,
        ),
        ("app.py", "class Base: pass\nclass Derived(Base): pass\n", 1),
        (
            "lib.rs",
            "trait Task {}\nstruct Worker;\nimpl Task for Worker {}",
            1,
        ),
    ] {
        let parsed = syntax(path, source);
        let captured = parsed.facts().static_facts().expect("captured edge syntax");
        assert_eq!(captured.heritage_edges().len(), expected, "{path}");
    }
}

#[test]
fn test_edge_typescript_calls_follow_import_bindings_in_each_dialect() {
    for extension in ["ts", "tsx", "mts", "cts"] {
        let path = format!("app/main.{extension}");
        let source = "import { leaf as run } from '../lib/jobs';\nexport function root(): number { return run(); }\n";
        let result = resolution(
            "typescript",
            &[
                (path.as_str(), source),
                (
                    "lib/jobs.ts",
                    "export function leaf(): number { return 1; }",
                ),
            ],
            &[],
            WORK_MAX,
        );
        let edges = calls(&result, &path).collect::<Vec<_>>();
        assert_eq!(edges.len(), 1, "{extension}: {result:?}");
        assert_eq!(source_of(edges[0], source), "run()");
        assert_eq!(
            edges[0]
                .target_binding
                .as_ref()
                .expect("target declaration")
                .unit()
                .key()
                .as_str(),
            "lib/jobs.ts"
        );
    }
}

#[test]
fn test_edge_namespace_imports_resolve_calls_and_heritage() {
    let source = "import * as tasks from './jobs.ts';\nexport function root() { tasks.leaf(); }\nclass Derived extends tasks.Base {}";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("jobs.ts", "export function leaf() {}\nexport class Base {}"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.ts").count(), 1);
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Extends))
    );
}

#[test]
fn test_edge_python_calls_follow_local_and_import_bindings() {
    let source = "from jobs import leaf as run\nimport jobs as tasks\ndef local(): return 1\ndef root():\n    return run() + tasks.leaf() + local()\n";
    let result = resolution(
        "python",
        &[("main.py", source), ("jobs.py", "def leaf(): return 1\n")],
        &[],
        WORK_MAX,
    );
    let edges = calls(&result, "main.py").collect::<Vec<_>>();
    let authored = edges
        .iter()
        .map(|edge| source_of(edge, source))
        .collect::<Vec<_>>();
    assert_eq!(authored, ["run()", "tasks.leaf()", "local()"]);
    assert_eq!(
        edges[0]
            .target_binding
            .as_ref()
            .expect("target declaration")
            .unit()
            .key()
            .as_str(),
        "jobs.py"
    );
}

#[test]
fn test_edge_python_imports_keep_relative_bindings_and_reject_dotted_module_shadows() {
    let result = resolution(
        "python",
        &[
            (
                "pkg/main.py",
                "from .jobs import leaf as run\ndef root(): return run()\n",
            ),
            ("pkg/jobs.py", "def leaf(): return 1\n"),
            (
                "other.py",
                "import pkg.jobs\ndef root(): return pkg.leaf()\n",
            ),
            ("stub.pyi", "def declared(): ...\n"),
        ],
        &[],
        WORK_MAX,
    );
    let resolved = calls(&result, "pkg/main.py").collect::<Vec<_>>();
    assert_eq!(resolved.len(), 1, "{result:?}");
    assert_eq!(
        resolved[0]
            .target_binding
            .as_ref()
            .expect("target declaration")
            .unit()
            .key()
            .as_str(),
        "pkg/jobs.py"
    );
    assert_eq!(calls(&result, "other.py").count(), 0);
}

fn syntax(path: &str, text: &str) -> SyntaxDocument {
    let path = ProjectPath::new(path).expect("fixture path");
    let extension = path.as_str().rsplit_once('.').expect("fixture extension").1;
    let provider = registry::provider_for_extension(extension).expect("fixture provider");
    provider
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("fixture syntax")
}

fn resolve_unshared(
    graph: &rift_provider::NormalizedGraph,
    documents: &[StaticSource<'_>],
    namespace: &NamespaceInput<'_>,
    work_max: usize,
) -> StaticResolution {
    resolve(graph, documents, &[], namespace, work_max)
}

fn resolution(
    language: &str,
    sources: &[(&str, &str)],
    metadata: &[(&str, &str)],
    work_max: usize,
) -> StaticResolution {
    resolution_by(language, sources, metadata, work_max, &resolve_unshared)
}

/// A static pass over one fixture: what [`resolve`] receives and answers.
type Pass<'pass> = &'pass dyn Fn(
    &rift_provider::NormalizedGraph,
    &[StaticSource<'_>],
    &NamespaceInput<'_>,
    usize,
) -> StaticResolution;

/// Resolves the fixture with `pass`.
fn resolution_by(
    language: &str,
    sources: &[(&str, &str)],
    metadata: &[(&str, &str)],
    work_max: usize,
    pass: Pass<'_>,
) -> StaticResolution {
    let language = Language::from_identity_segment(language).expect("fixture language");
    let parsed = sources
        .iter()
        .map(|(path, text)| {
            let document = syntax(path, text);
            (document.path().clone(), document.into_facts())
        })
        .collect::<Vec<_>>();
    let metadata_paths = metadata
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("fixture metadata path"))
        .collect::<Vec<_>>();
    let context = metadata_paths
        .iter()
        .zip(metadata)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let selected = parsed
        .iter()
        .zip(sources)
        .map(|((path, facts), (_, source))| SelectedFile {
            path,
            source,
            syntax: facts,
        })
        .collect::<Vec<_>>();
    let package_sources = selected
        .iter()
        .map(|source| PackageSource::new(source.path, source.source))
        .collect::<Vec<_>>();
    let input = NamespaceInput::project(
        &SymbolOwner::Local,
        &language,
        &package_sources,
        &context,
        &[],
        ExactPackageLimits::new(32, 32_768),
    )
    .expect("fixture namespace");
    let mut namespaces = prepare_context(&input, &selected);
    let mut placed = selected
        .iter()
        .map(|source| PlacedFacts {
            facts: source.syntax,
            path: source.path,
            placement: DocumentPlacement::project_path(source.path)
                .expect("fixture placement")
                .with_identity_anchors(
                    namespaces
                        .anchors
                        .remove(source.path.as_str())
                        .unwrap_or_default(),
                )
                .with_module(namespaces.rust_modules.remove(source.path.as_str()))
                .with_logical_declarations(
                    namespaces
                        .mappings
                        .remove(source.path.as_str())
                        .unwrap_or_default(),
                ),
        })
        .collect::<Vec<_>>();
    if language.name == "rust" {
        let placements = placed
            .iter()
            .map(|placed| &placed.placement)
            .collect::<Vec<_>>();
        let mut aliases = crate::analyzer::namespace::placed_aliases(
            &input,
            &mut namespaces,
            &selected,
            &placements,
            1024,
            || panic!("invalid fixture export"),
        )
        .expect("fixture aliases");
        for placed in &mut placed {
            placed.placement = placed
                .placement
                .clone()
                .with_aliases(aliases.remove(placed.path.as_str()).unwrap_or_default());
        }
    }
    let semantics = WorkspaceSemantics::build_facts_placed(&placed, 1024, 7, None)
        .expect("fixture graph")
        .semantics;
    let documents = placed
        .iter()
        .zip(&selected)
        .map(|(placed, source)| StaticSource {
            path: source.path,
            source: source.source,
            facts: source.syntax,
            placement: &placed.placement,
        })
        .collect::<Vec<_>>();
    pass(semantics.graph(), &documents, &input, work_max)
}

fn calls<'source>(
    result: &'source StaticResolution,
    path: &'source str,
) -> impl Iterator<Item = &'source StaticOccurrence> {
    let unit = format!("rift://source/project/{path}");
    result.occurrences.iter().filter(move |occurrence| {
        occurrence.role == ReferenceRole::Call && occurrence.binding.unit().to_string() == unit
    })
}

/// Every resolved occurrence as (label, source identity, target identity); the label is `call`
/// for calls, otherwise the edge kind or the role.
fn labelled(result: &StaticResolution) -> Vec<(String, String, String)> {
    result
        .occurrences
        .iter()
        .map(|occurrence| {
            let label = match (occurrence.role, occurrence.edge_kind) {
                (ReferenceRole::Call, _) => "call".to_owned(),
                (_, Some(kind)) => format!("{kind:?}"),
                (role, None) => format!("{role:?}"),
            };
            (
                label,
                occurrence
                    .edge_from
                    .as_ref()
                    .map(|from| from.as_str().to_owned())
                    .unwrap_or_default(),
                target(occurrence).to_owned(),
            )
        })
        .collect()
}

fn target(occurrence: &StaticOccurrence) -> &str {
    match &occurrence.target {
        NormalizedTarget::Symbol(symbol) => symbol.as_str(),
        NormalizedTarget::Contribution(reference) => reference.symbol().as_str(),
    }
}

fn source_of<'source>(occurrence: &StaticOccurrence, source: &'source str) -> &'source str {
    let range = occurrence.binding.range();
    let start = usize::try_from(range.start()).expect("fixture range start");
    let end = usize::try_from(range.end()).expect("fixture range end");
    &source[start..end]
}

#[test]
fn javascript_calls_follow_local_import_aliases_and_keep_each_occurrence() {
    let source = "import { launch as run } from './barrel.mjs';\n\
                  function caller() { run(); run(); { const run = 0; run(); } }";
    let result = resolution(
        "javascript",
        &[
            ("app/main.mjs", source),
            (
                "app/barrel.mjs",
                "export { start as launch } from '../lib/jobs.mjs';",
            ),
            ("app/decoy.mjs", "export function launch() {}"),
            ("lib/jobs.mjs", "export function start() {}"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(result.analyzed_sources, 4);
    assert_eq!(result.unavailable_sources, 0);
    assert_eq!(result.unresolved_sites, 1);
    assert!(!result.exhausted);
    let calls = calls(&result, "app/main.mjs").collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].binding.range(), calls[1].binding.range());
    for call in calls {
        assert_eq!(source_of(call, source), "run()");
        assert_eq!(
            target(call),
            "rift://symbol/local/javascript/.%2Flib%2Fjobs.mjs/start"
        );
        assert_eq!(
            call.source_digest,
            rift_core::FileDigest::of(source.as_bytes())
        );
    }
    assert!(result.occurrences.iter().any(|occurrence| {
        occurrence.role == ReferenceRole::Import && source_of(occurrence, source) == "run"
    }));
}

#[test]
fn javascript_default_import_and_local_class_method_keep_authored_targets() {
    let source = "import execute from './other.mjs';\n\
                  class Task { run() {} static wrong() {} }\n\
                  function caller() { const task = new Task(); task.run(); task.wrong(); execute(); }";
    let result = resolution(
        "javascript",
        &[
            ("main.mjs", source),
            ("other.mjs", "export default function external() {}"),
        ],
        &[],
        WORK_MAX,
    );
    let calls = calls(&result, "main.mjs")
        .map(|occurrence| (source_of(occurrence, source), target(occurrence)))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            (
                "new Task()",
                "rift://symbol/local/javascript/.%2Fmain.mjs/Task"
            ),
            (
                "task.run()",
                "rift://symbol/local/javascript/.%2Fmain.mjs/Task/run"
            ),
            (
                "execute()",
                "rift://symbol/local/javascript/.%2Fother.mjs/external"
            ),
        ]
    );
}

#[test]
fn javascript_rebinding_and_class_prototype_mutation_leave_calls_unresolved() {
    let source = "function original() {}\n\
                  function replacement() {}\n\
                  class Task { run() {} }\n\
                  original = replacement;\n\
                  Task.prototype.run = replacement;\n\
                  original(); const task = new Task(); task.run();";
    let result = resolution("javascript", &[("main.mjs", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.mjs")
            .map(|edge| source_of(edge, source))
            .collect::<Vec<_>>(),
        ["new Task()"]
    );
    assert_eq!(
        result
            .edge_calls
            .values()
            .map(|coverage| coverage.unresolved.len())
            .sum::<usize>(),
        2
    );
}

#[test]
fn javascript_module_level_occurrence_keeps_its_physical_target() {
    let source = "import { run } from './tasks.mjs'; run();";
    let tasks = "export function run() {}";
    let result = resolution(
        "javascript",
        &[("main.mjs", source), ("tasks.mjs", tasks)],
        &[],
        WORK_MAX,
    );
    let mut calls = calls(&result, "main.mjs");
    let call = calls.next().expect("module-level call");
    assert_eq!(source_of(call, source), "run()");
    assert_eq!(
        call.target_binding
            .as_ref()
            .expect("target declaration")
            .unit()
            .to_string(),
        "rift://source/project/tasks.mjs"
    );
    let range = call
        .target_binding
        .as_ref()
        .expect("target declaration")
        .range();
    let start = usize::try_from(range.start()).expect("fixture range start");
    let end = usize::try_from(range.end()).expect("fixture range end");
    assert_eq!(&tasks[start..end], "function run() {}");
    assert!(calls.next().is_none());
}

#[test]
fn javascript_literal_require_resolves_only_with_commonjs_mode_and_unshadowed_binding() {
    let source = "const run = require('./jobs.cjs').start;\n\
                  function caller() { run(); }\n\
                  function decoy(require) { const fake = require('./jobs.cjs').start; fake(); }";
    let result = resolution(
        "javascript",
        &[
            ("main.cjs", source),
            (
                "jobs.cjs",
                "function start() {}\nmodule.exports = { start };",
            ),
        ],
        &[],
        WORK_MAX,
    );
    let calls = calls(&result, "main.cjs")
        .map(|occurrence| (source_of(occurrence, source), target(occurrence)))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [("run()", "rift://symbol/local/javascript/.%2Fjobs.cjs/start")]
    );
}

#[test]
fn javascript_js_modules_follow_the_nearest_explicit_package_mode() {
    let source = "const run = require('../jobs.js').start; run();";
    let files = [
        ("common/main.js", source),
        ("esm/main.js", source),
        (
            "jobs.js",
            "function start() {}\nmodule.exports = { start };",
        ),
    ];
    let common = r#"{"type":"commonjs"}"#;
    let esm = r#"{"type":"module"}"#;
    let result = resolution(
        "javascript",
        &files,
        &[("package.json", common), ("esm/package.json", esm)],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "common/main.js").count(), 1);
    assert_eq!(calls(&result, "esm/main.js").count(), 0);
    let missing = resolution("javascript", &files, &[], WORK_MAX);
    assert!(
        !missing
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Call)
    );
}

#[test]
fn test_typescript_merged_type_alias_keeps_raw_property_containment() {
    let source = "declare namespace Config {
  namespace Options { export type Mode = 'strict'; }
  export type Options = {
    charset?: string;
    allowArbitraryExtensions?: boolean;
  };
  namespace Ambiguous { export type Yes = true; }
  namespace Ambiguous { export type No = false; }
  export type Ambiguous = { other?: string };
}
";
    let path = "main.d.ts";
    let result = resolution("typescript", &[(path, source)], &[], WORK_MAX);
    for (container, name, declaration) in [
        ("Options", "charset", "charset?: string"),
        (
            "Options",
            "allowArbitraryExtensions",
            "allowArbitraryExtensions?: boolean",
        ),
    ] {
        let child = format!("/Config/{container}/{name}");
        let expected_parent = format!("/Config/{container}");
        let mut matches = result.occurrences.iter().filter(|edge| {
            edge.edge_kind == Some(rift_protocol::read::EdgeKind::Contains)
                && target(edge).ends_with(&child)
        });
        let edge = matches.next().expect("stored containment occurrence");
        assert!(matches.next().is_none());
        assert!(
            edge.edge_from
                .as_ref()
                .expect("authored alias container")
                .as_str()
                .ends_with(&expected_parent)
        );
        assert_eq!(source_of(edge, source), name);
        let binding = edge
            .target_binding
            .as_ref()
            .expect("authored property binding");
        assert_eq!(binding.unit().key().as_str(), path);
        let start = usize::try_from(binding.range().start()).expect("property start");
        let end = usize::try_from(binding.range().end()).expect("property end");
        assert_eq!(&source[start..end], declaration);
    }

    for (suffix, name) in [
        ("/Config/Options/Mode", "Mode"),
        ("/Config/Ambiguous/other", "other"),
    ] {
        let edge = result
            .occurrences
            .iter()
            .find(|edge| {
                edge.edge_kind == Some(rift_protocol::read::EdgeKind::Contains)
                    && target(edge).ends_with(suffix)
                    && source_of(edge, source) == name
            })
            .expect("existing fallback containment");
        assert!(
            edge.edge_from
                .as_ref()
                .expect("file module")
                .as_str()
                .ends_with("main.d.ts")
        );
    }
}

#[test]
fn javascript_empty_analysis_completes_without_an_unresolved_site() {
    let result = resolution(
        "javascript",
        &[("empty.mjs", "export function idle() {}")],
        &[],
        WORK_MAX,
    );
    assert_eq!(result.occurrences.len(), 1);
    assert_eq!(
        result.occurrences[0].edge_kind,
        Some(rift_protocol::read::EdgeKind::Contains)
    );
    assert_eq!(result.analyzed_sources, 1);
    assert_eq!(result.unavailable_sources, 0);
    assert_eq!(result.unresolved_sites, 0);
    assert!(!result.exhausted);
}

/// Resolves the files one after another in path order, each with what is left of the pass
/// budget or its own share, whichever is less: the pass the join of separately resolved
/// files must equal.
fn resolved_in_one_pass(
    file_work: super::FileWork,
    graph: &rift_provider::NormalizedGraph,
    documents: &[StaticSource<'_>],
    input: &NamespaceInput<'_>,
    work_max: usize,
) -> StaticResolution {
    super::resolve_by(
        graph,
        (documents, &[]),
        input,
        work_max,
        |index, work, resolution| {
            for file in index.files.values() {
                if !work.charge() {
                    break;
                }
                let left = work.budget.remaining();
                let share = file_work.max(file);
                let apart = index.resolve_file_apart(file, left.min(share), &resolution.edge_calls);
                work.exhausted = apart.exhausted && left < share;
                let over_share = apart.exhausted && !work.exhausted;
                work.budget = rift_core::LoopBudget::new(left - apart.used);
                resolution.join(file, apart.resolution, over_share);
                if work.exhausted {
                    break;
                }
            }
        },
    )
}

/// Resolves the files across the pool with `file_work` as each file's share.
fn resolved_apart(
    file_work: super::FileWork,
    graph: &rift_provider::NormalizedGraph,
    documents: &[StaticSource<'_>],
    input: &NamespaceInput<'_>,
    work_max: usize,
) -> StaticResolution {
    super::resolve_by(
        graph,
        (documents, &[]),
        input,
        work_max,
        |index, work, resolution| index.resolve_files(file_work, work, resolution),
    )
}

const APART_SOURCES: [(&str, &str); 4] = [
    (
        "a.ts",
        "import { leaf } from './b.js'; export function root(input: Box) { leaf(); input.read(); eval('x'); } interface Box { read(): void; }",
    ),
    (
        "b.ts",
        "export function leaf() { helper(); helper(); } function helper() { const value = { count: 1 }; return value.count; }",
    ),
    (
        "c.ts",
        "import { root } from './a.js'; class Base { run() {} } class Derived extends Base { run() { root(undefined as never); this.run(); } }",
    ),
    ("d.ts", "function {"),
];

#[test]
fn files_resolved_apart_equal_one_pass_under_every_budget() {
    let captured = super::FileWork::CAPTURED;
    let printed = |work_max, pass: Pass<'_>| {
        let result = resolution_by("typescript", &APART_SOURCES, &[], work_max, pass);
        (result.exhausted, format!("{result:?}"))
    };
    let one_pass: Pass<'_> = &|graph, documents, input, work_max| {
        resolved_in_one_pass(captured, graph, documents, input, work_max)
    };
    let mut budgets = 0_usize;
    for work_max in 0..WORK_MAX {
        let (exhausted, expected) = printed(work_max, one_pass);
        assert_eq!(
            printed(work_max, &resolve_unshared).1,
            expected,
            "work_max={work_max}"
        );
        budgets += 1;
        if !exhausted {
            break;
        }
    }
    // The sweep covers a budget ending inside the index, inside each file, and none.
    assert!(budgets > APART_SOURCES.len(), "budgets={budgets}");
    let (exhausted, complete) = printed(WORK_MAX, one_pass);
    assert!(!exhausted);
    assert_eq!(printed(WORK_MAX, &resolve_unshared).1, complete);
    assert!(complete.contains("role: Call"), "{complete}");
}

#[test]
fn a_name_bound_in_many_scopes_resolves_to_its_innermost_binding() {
    // More bindings of one name than a lookup reads in declaration order, in sibling and
    // nested scopes, so the lookup goes through the scopes around each read.
    let functions = 24;
    let mut source = String::from("const e = 0; e;\n");
    for function in 0..functions {
        writeln!(
            source,
            "function f{function}() {{ const e = {function}; e; function inner() {{ e; }} function own() {{ const e = 1; e; }} }}"
        )
        .expect("a string accepts formatted text");
    }
    let result = resolution("typescript", &[("main.ts", &source)], &[], 1_000_000);
    assert!(!result.exhausted);
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read)
        .map(|edge| {
            target(edge)
                .rsplit_once(".%2Fmain.ts/")
                .expect("local target")
                .1
        })
        .collect::<Vec<_>>();
    let mut expected = vec!["e".to_owned()];
    for function in 0..functions {
        expected.push(format!("f{function}/e"));
        expected.push(format!("f{function}/e"));
        expected.push(format!("f{function}/own/e"));
    }
    assert_eq!(reads, expected);
}

#[test]
fn many_conditional_items_leave_only_the_inactive_ones_out() {
    // More conditional items than a lookup reads in order: nested and sibling items.
    let mut source = String::from("fn leaf() {}\n");
    for item in 0..24 {
        writeln!(
            source,
            "#[cfg(feature=\"off\")] fn off{item}() {{ leaf(); }}\n#[cfg(feature=\"std\")] mod on{item} {{ pub fn call() {{ crate::leaf(); }} #[cfg(feature=\"off\")] pub fn skip() {{ crate::leaf(); }} }}"
        )
        .expect("a string accepts formatted text");
    }
    source.push_str("fn root() { leaf(); }\n");
    let manifest =
        "[package]\nname='fixture'\nversion='0.1.0'\n[features]\ndefault=['std']\nstd=[]\noff=[]\n";
    let result = resolution(
        "rust",
        &[("src/lib.rs", &source)],
        &[("Cargo.toml", manifest)],
        1_000_000,
    );
    assert!(!result.exhausted);
    let found = calls(&result, "src/lib.rs").collect::<Vec<_>>();
    // One call from each active module and one from `root`; none from an inactive item.
    assert_eq!(found.len(), 25);
    assert!(
        found
            .iter()
            .all(|call| target(call) == "rift://symbol/local/rust/fixture/leaf")
    );
}

#[test]
fn work_done_once_the_budget_ends_stays_inside_the_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let files = (0..24)
        .map(|file| format!("f{file:02}.ts"))
        .collect::<Vec<_>>();
    let source = "function leaf() {} function root() { leaf(); leaf(); leaf(); }";
    let sources = files
        .iter()
        .map(|path| (path.as_str(), source))
        .collect::<Vec<_>>();
    let left = AtomicUsize::new(0);
    let performed = AtomicUsize::new(0);
    let measured = |work_max| {
        resolution_by(
            "typescript",
            &sources,
            &[],
            work_max,
            &|graph, documents, input, work_max| {
                super::resolve_by(
                    graph,
                    (documents, &[]),
                    input,
                    work_max,
                    |index, work, resolution| {
                        left.store(work.budget.remaining(), Ordering::Relaxed);
                        index.resolve_files(super::FileWork::CAPTURED, work, resolution);
                        performed.store(index.performed.load(Ordering::Relaxed), Ordering::Relaxed);
                    },
                )
            },
        )
    };
    let complete = measured(1_000_000);
    assert!(!complete.exhausted);
    assert_eq!(complete.analyzed_sources, files.len());
    let needed = performed.load(Ordering::Relaxed);
    let index = 1_000_000 - left.load(Ordering::Relaxed);
    // Budgets that end from inside the first file to inside the last.
    let mut cut_early = 0;
    for work_max in (index..index + needed).step_by(needed / 64 + 1) {
        let cut = measured(work_max);
        let left = left.load(Ordering::Relaxed);
        let performed = performed.load(Ordering::Relaxed);
        assert!(
            performed <= left,
            "work_max={work_max}, performed={performed}, left={left}"
        );
        if cut.exhausted && cut.analyzed_sources < files.len() / 4 {
            cut_early += 1;
            // The files after the cut start no pass: the work stays with the files joined.
            assert!(
                performed < needed / 2,
                "work_max={work_max}, performed={performed}, needed={needed}"
            );
        }
    }
    assert!(cut_early > 0);
}

#[test]
fn a_file_past_its_share_reports_its_unit_and_later_files_keep_their_facts() {
    let complete = resolution("typescript", &APART_SOURCES, &[], WORK_MAX);
    assert!(!complete.exhausted);
    let unavailable = |result: &StaticResolution| {
        result
            .edge_calls
            .iter()
            .filter(|(_, coverage)| coverage.unavailable)
            .map(|(unit, _)| format!("{unit:?}"))
            .collect::<Vec<_>>()
    };
    let calls_in = |result: &StaticResolution, path| calls(result, path).count();
    let mut kept_later_files = false;
    // Every floor from none to one that holds each file: the join equals the pass in
    // order, the pass budget is never the bound, and what a file loses stays its own.
    for floor in 0..WORK_MAX {
        let file_work = super::FileWork {
            floor,
            per_element: 0,
        };
        let apart = resolution_by(
            "typescript",
            &APART_SOURCES,
            &[],
            WORK_MAX,
            &|graph, documents, input, work_max| {
                resolved_apart(file_work, graph, documents, input, work_max)
            },
        );
        let one_pass = resolution_by(
            "typescript",
            &APART_SOURCES,
            &[],
            WORK_MAX,
            &|graph, documents, input, work_max| {
                resolved_in_one_pass(file_work, graph, documents, input, work_max)
            },
        );
        assert_eq!(
            format!("{apart:?}"),
            format!("{one_pass:?}"),
            "floor={floor}"
        );
        assert!(!apart.exhausted, "floor={floor}");
        let over = unavailable(&apart);
        kept_later_files |= over.iter().any(|unit| unit.contains("a.ts"))
            && !over.iter().any(|unit| unit.contains("b.ts"))
            && calls_in(&apart, "b.ts") == calls_in(&complete, "b.ts")
            && calls_in(&apart, "b.ts") > 0;
        if unavailable(&apart) == unavailable(&complete) {
            assert_eq!(
                format!("{apart:?}"),
                format!("{complete:?}"),
                "floor={floor}"
            );
            break;
        }
    }
    assert!(kept_later_files);
}

#[test]
fn javascript_cycles_malformed_sources_and_work_exhaustion_remain_distinct() {
    let sources = [
        ("main.mjs", "import { start } from './cycle.mjs'; start();"),
        ("cycle.mjs", "export { start } from './cycle.mjs';"),
        ("bad.mjs", "function {"),
    ];
    let result = resolution("javascript", &sources, &[], WORK_MAX);
    assert!(
        !result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Call)
    );
    assert_eq!(result.analyzed_sources, 2);
    assert_eq!(result.unavailable_sources, 1);
    // The import, invocation, and callee read each remain unresolved.
    assert_eq!(result.unresolved_sites, 3);
    assert!(!result.exhausted);
    let exhausted = resolution("javascript", &sources, &[], 1);
    assert!(exhausted.exhausted);
    assert_eq!(exhausted.analyzed_sources, 0);
}

#[test]
fn rust_uses_admitted_module_paths_and_excludes_conditional_candidates() {
    let source = "mod tasks;\n\
                  use crate::tasks::run as start;\n\
                  struct Job;\n\
                  trait Other { fn wrong(&self); }\n\
                  impl Other for Job { fn wrong(&self) {} }\n\
                  impl Job { fn execute(&self) {} }\n\
                  #[cfg(test)] fn hidden() {}\n\
                  fn caller(job: Job) { start(); crate::tasks::run(); job.execute(); job.wrong(); hidden(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source), ("src/tasks.rs", "pub fn run() {}")],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let calls = calls(&result, "src/lib.rs")
        .map(|occurrence| (source_of(occurrence, source), target(occurrence)))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            ("start()", "rift://symbol/local/rust/fixture/tasks/run"),
            (
                "crate::tasks::run()",
                "rift://symbol/local/rust/fixture/tasks/run"
            ),
            (
                "job.execute()",
                "rift://symbol/local/rust/fixture/Job/execute"
            ),
            ("job.wrong()", "rift://symbol/local/rust/fixture/Job/wrong"),
        ]
    );
    assert!(!result.exhausted);
}

#[test]
fn rust_qualified_calls_read_the_written_module_and_type_tokens() {
    let source = "pub mod definitions {\n\
        pub fn begin() {}\n\
        pub struct Wrapper<T>(pub T);\n\
        impl Wrapper<u8> { pub fn make() {} }\n\
        pub mod nested { pub fn from_parent() { super::begin(); } }\n\
    }\n\
    use crate::definitions as imported;\n\
    pub fn run() {\n\
        crate::definitions::begin();\n\
        self::definitions::begin();\n\
        imported::begin();\n\
        crate::definitions::Wrapper::<u8>::make();\n\
    }\n\
    pub fn unrelated<Wrapper>(value: u8) {\n\
        unknown_module::begin();\n\
        Wrapper::make();\n\
        value.to_string();\n\
    }\n\
    pub fn shadowed() {\n\
        struct imported;\n\
        impl imported { fn begin() {} }\n\
        imported::begin();\n\
    }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let read = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .map(|edge| (source_of(edge, source), target(edge)))
        .collect::<Vec<_>>();
    let module = "rift://symbol/local/rust/fixture/definitions";
    assert_eq!(
        read,
        [
            ("super", module),
            ("definitions", module),
            ("definitions", module),
            ("imported", module),
            ("definitions", module),
            (
                "Wrapper",
                "rift://symbol/local/rust/fixture/definitions/Wrapper"
            ),
        ]
    );
    let controls = u64::try_from(source.find("pub fn unrelated").expect("control boundary"))
        .expect("control offset");
    let leaf = "rift://symbol/local/rust/fixture/definitions/begin";
    assert_eq!(
        calls(&result, "src/lib.rs")
            .filter(|call| call.binding.range().start() < controls)
            .map(|call| (source_of(call, source), target(call)))
            .collect::<Vec<_>>(),
        [
            ("super::begin()", leaf),
            ("crate::definitions::begin()", leaf),
            ("self::definitions::begin()", leaf),
            ("imported::begin()", leaf),
            (
                "crate::definitions::Wrapper::<u8>::make()",
                "rift://symbol/local/rust/fixture/definitions/Wrapper/make"
            ),
        ],
    );
    assert!(!result.ended);
}

#[test]
fn rust_writes_value_path_terminal_reads_without_competing_call_facts() {
    let source = "pub mod values {\n\
        pub const MAX: u8 = 4;\n\
        pub static INITIAL: u8 = 2;\n\
        pub enum Choice { Plain, Wrapped(u8) }\n\
        pub struct Flag;\n\
        pub struct Signal(pub u8);\n\
        pub fn leaf() -> u8 { 1 }\n\
        pub fn constructors() { let _ = Signal; }\n\
    }\n\
    use crate::values::leaf as function;\n\
    pub fn examine(choice: values::Choice) -> u8 {\n\
        let _ = values::MAX;\n\
        let _ = values::INITIAL;\n\
        let _ = values::Choice::Plain;\n\
        let _ = values::Choice::Wrapped;\n\
        let _ = values::leaf;\n\
        let _ = values::leaf();\n\
        let _ = function;\n\
        let _ = values::Flag;\n\
        match choice {\n\
            values::Choice::Plain => 0,\n\
            values::Choice::Wrapped(value) => value,\n\
        }\n\
    }\n\
    pub fn shadowed(function: fn() -> u8) { let _ = function; }\n\
    pub fn through_glob() -> u8 { use values::*; MAX }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let read = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && target(edge) != "rift://symbol/local/rust/fixture/values"
        })
        .map(|edge| {
            (
                source_of(edge, source),
                target(edge)
                    .strip_prefix("rift://symbol/local/rust/fixture/values/")
                    .expect("resolved authored value"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        read,
        [
            ("Signal", "Signal"),
            ("MAX", "MAX"),
            ("INITIAL", "INITIAL"),
            ("Plain", "Choice/Plain"),
            ("Wrapped", "Choice/Wrapped"),
            ("leaf", "leaf"),
            ("function", "leaf"),
            ("Flag", "Flag"),
            ("Plain", "Choice/Plain"),
            ("Wrapped", "Choice/Wrapped"),
        ],
    );
    assert_eq!(
        calls(&result, "src/lib.rs")
            .map(|call| source_of(call, source))
            .collect::<Vec<_>>(),
        ["values::leaf()"],
    );
    assert_eq!(
        result.omitted.get(&rift_protocol::read::EdgeKind::Reads),
        Some(&1)
    );
    assert!(!result.ended);
}

#[test]
fn rust_value_reads_respect_local_item_block_scopes() {
    let source = r"
const RATE: u8 = 1;
static PULSE: u8 = 2;
struct Maker(pub u8);
struct Flag;
fn named() -> u8 { 1 }
mod keys { pub const OTHER: u8 = 3; }
use keys::OTHER as IMPORTED;
fn ordinary() -> u8 { let _ = RATE; PULSE }
fn bare_const() -> u8 { const RATE: u8 = 9; RATE }
fn bare_static() -> u8 { static PULSE: u8 = 9; PULSE }
fn via_import() -> u8 { const IMPORTED: u8 = 9; IMPORTED }
fn sibling() -> u8 { let _ = { const RATE: u8 = 9; RATE }; { RATE } }
fn qualified() -> u8 { const RATE: u8 = 9; crate::RATE }
fn imported() -> u8 { IMPORTED }
fn before_declaration() -> u8 { let result = RATE; const RATE: u8 = 9; result }
fn inner_import() -> u8 { const IMPORTED: u8 = 9; { use crate::keys::OTHER as IMPORTED; IMPORTED } }
fn inactive_item() -> u8 { #[cfg(any())] const RATE: u8 = 9; RATE }
fn local_tuple() { struct Maker(u16); let _ = Maker; }
fn local_unit() { struct Flag; let _ = Flag; }
fn module_constructors() { let _ = Maker; let _ = Flag; }
fn type_only() -> u8 { struct named { field: u16 } let f = named; f() }
fn function_shadow() -> u8 { fn named() -> u8 { 9 } let f = named; f() }
fn value_shadow(named: u8) -> u8 { let local = named; local }
";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let seen = result
        .occurrences
        .iter()
        .take(
            result
                .rust_import_qualifier_start
                .unwrap_or(result.occurrences.len()),
        )
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .map(|edge| {
            (
                edge.binding.range().start(),
                target(edge)
                    .strip_prefix("rift://symbol/local/rust/fixture/")
                    .expect("package declaration"),
            )
        })
        .collect::<Vec<_>>();
    let position = |statement, token: &str| {
        let line = source
            .lines()
            .find(|line| line.starts_with(statement))
            .expect("fixture line");
        let start = source.find(line).expect("fixture line start");
        u64::try_from(start + line.rfind(token).expect("read token")).expect("fixture offset")
    };
    assert_eq!(
        seen,
        [
            (position("fn ordinary", "RATE"), "RATE"),
            (position("fn ordinary", "PULSE"), "PULSE"),
            (position("fn sibling", "RATE"), "RATE"),
            (position("fn qualified", "RATE"), "RATE"),
            (position("fn imported", "IMPORTED"), "keys/OTHER"),
            (position("fn inner_import", "IMPORTED"), "keys/OTHER"),
            (position("fn inactive_item", "RATE"), "RATE"),
            (position("fn module_constructors", "Maker"), "Maker"),
            (position("fn module_constructors", "Flag"), "Flag"),
            (position("fn type_only", "named"), "named"),
        ],
    );
}

#[test]
fn rust_value_reads_exclude_initializer_local_functions() {
    let source = r"
pub fn named() -> u8 { 1 }
pub const CHOICE: fn() -> u16 = {
    fn named() -> u16 { 9 }
    let constant = named;
    constant
};
pub static OTHER: fn() -> u32 = {
    fn named() -> u32 { 8 }
    let stored = named;
    stored
};
pub fn root_value() -> fn() -> u8 { let root = named; root }
pub mod inside {
    fn named() -> u64 { 4 }
    pub fn inline_value() -> fn() -> u64 { let inline = named; inline }
}
";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let seen = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .map(|edge| (edge.binding.range().start(), target(edge)))
        .collect::<Vec<_>>();
    let position = |text: &str| {
        u64::try_from(
            source.find(text).expect("selected function binding") + text.len() - "named".len(),
        )
        .expect("fixture offset")
    };
    assert_eq!(
        seen,
        [
            (
                position("let root = named"),
                "rift://symbol/local/rust/fixture/named"
            ),
            (
                position("let inline = named"),
                "rift://symbol/local/rust/fixture/inside/named"
            ),
        ]
    );
}

#[test]
fn rust_root_macro_calls_keep_the_export_declaration_and_binding_guard() {
    let source = r#"
mod macros;
macro_rules! wire { () => { 1u8 }; }
macro_rules! wrap { ($value:expr) => { $value }; }
pub fn leaf() -> u8 { 2 }
pub fn read() -> u8 { leaf() + crate::wire!() }
pub fn direct() -> u8 { wire!() }
pub fn nested() -> u8 { wrap!(crate::wire!()) }
pub fn standard() -> usize { std::concat!("a", "b").len() }
pub fn disabled() -> u8 { crate::inactive!() }
pub fn private() -> u8 { crate::local!() }
"#;
    let definitions = r"
#[macro_export]
macro_rules! wire { () => { 3u8 }; }
#[cfg(any())]
#[macro_export]
macro_rules! inactive { () => { 4u8 }; }
macro_rules! local { () => { 5u8 }; }
";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source), ("src/macros.rs", definitions)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let observed = calls(&result, "src/lib.rs")
        .map(|edge| (source_of(edge, source), target(edge)))
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        [
            ("leaf()", "rift://symbol/local/rust/fixture/leaf"),
            (
                "crate::wire!()",
                "rift://symbol/local/rust/fixture/macros/wire"
            ),
            ("wire!()", "rift://symbol/local/rust/fixture/wire"),
            (
                "wrap!(crate::wire!())",
                "rift://symbol/local/rust/fixture/wrap",
            ),
        ]
    );
    let (supplemental, _) = result.supplemental_start.expect("deferred macro site");
    let row = result.occurrences[supplemental..]
        .iter()
        .find(|edge| edge.role == ReferenceRole::Call)
        .expect("supplemental macro call");
    assert_eq!(source_of(row, source), "crate::wire!()");
    let start =
        u64::try_from(source.find("crate::wire!()").expect("macro call")).expect("source offset");
    assert!(result.edge_calls.values().any(|coverage| {
        !coverage.unavailable && coverage.unresolved.iter().any(|range| range.start == start)
    }));
    assert!(!result.ended);
}

#[test]
fn rust_textual_macro_calls_respect_order_and_physical_sibling_scopes() {
    let source = r"
macro_rules! picked { () => { 1u8 }; }
pub const fn earlier() -> u8 { picked!() }
pub const fn scoped() -> u8 {
    let before = picked!();
    macro_rules! picked { () => { 2u8 }; }
    let inside = picked!();
    let nested = {
        macro_rules! picked { () => { 3u8 }; }
        picked!()
    };
    before + inside + nested + picked!()
}
pub const fn sibling() -> u8 { picked!() }
";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let declaration = |literal| {
        u64::try_from(source.find(literal).expect("macro definition")).expect("macro byte offset")
    };
    let outer = declaration("macro_rules! picked { () => { 1u8");
    let inner = declaration("macro_rules! picked { () => { 2u8");
    let nested = declaration("macro_rules! picked { () => { 3u8");
    let observed = calls(&result, "src/lib.rs")
        .map(|call| {
            assert_eq!(source_of(call, source), "picked!()");
            call.target_binding
                .as_ref()
                .expect("physical macro")
                .range()
                .start()
        })
        .collect::<Vec<_>>();
    assert_eq!(observed, [outer, outer, inner, nested, inner, outer]);
    assert!(!result.ended);
}

#[test]
fn rust_textual_macro_calls_wait_for_later_proven_definitions_after_opacity() {
    let source = r"
macro_rules! selected { () => { 1u8 }; }
pub const fn before() -> u8 { selected!() }

macro_rules! install { () => { const OTHER: u8 = 9; }; }
install!();
pub const fn after_expansion() -> u8 { selected!() }

macro_rules! selected { () => { 2u8 }; }
pub const fn restored() -> u8 { selected!() }

#[macro_use]
mod brought_in { macro_rules! selected { () => { 7u8 }; } }
pub const fn after_module() -> u8 { selected!() }

macro_rules! selected { () => { 3u8 }; }
#[cfg(any())]
mod absent;
pub const fn after_inactive() -> u8 { selected!() }

mod file_backed;
pub const fn after_file() -> u8 { selected!() }

macro_rules! selected { () => { 4u8 }; }
pub const fn recovered() -> u8 { selected!() }
";
    let result = resolution(
        "rust",
        &[
            ("src/lib.rs", source),
            (
                "src/file_backed.rs",
                "#![macro_use]\nmacro_rules! selected { () => { 8u8 }; }",
            ),
        ],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let observed = calls(&result, "src/lib.rs")
        .filter(|call| source_of(call, source) == "selected!()")
        .map(|call| call.binding.range().start())
        .collect::<Vec<_>>();
    let site = |owner| {
        let start = source.find(owner).expect("authored function");
        let offset = source[start..].find("selected!()").expect("authored call");
        u64::try_from(start + offset).expect("call byte offset")
    };
    assert_eq!(
        observed,
        [
            site("fn before"),
            site("fn restored"),
            site("fn after_inactive"),
            site("fn recovered"),
        ]
    );
    assert!(!result.ended);
}

#[test]
fn rust_literal_and_pattern_field_labels_keep_physical_owners_and_roles() {
    let source = "pub struct Left { pub x: u8, pub y: u8 }\n\
        pub struct Right { pub x: u8, pub y: u8 }\n\
        pub enum State { Named { x: u8 } }\n\
        pub fn left(x: u8) -> Left { Left { x, y: 1 } }\n\
        pub fn right(x: u8) -> Right { Right { x: x, y: 1 } }\n\
        pub fn unpack(left: Left) -> u8 { let Left { x, y: other } = left; x + other }\n\
        pub fn state(x: u8) -> u8 {\n\
            let state = State::Named { x };\n\
            match state { State::Named { x: value } => value }\n\
        }\n\
        pub fn direct_left(left: Left) -> u8 { left.x }\n\
        pub fn direct_right(right: Right) -> u8 { right.x }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let accesses = |role| {
        result
            .occurrences
            .iter()
            .filter(|edge| {
                edge.role == role
                    && edge.edge_kind.is_none()
                    && (target(edge).ends_with("/x") || target(edge).ends_with("/y"))
            })
            .map(|edge| {
                (
                    source_of(edge, source),
                    target(edge)
                        .strip_prefix("rift://symbol/local/rust/fixture/")
                        .expect("owned Rust field"),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        accesses(ReferenceRole::Write),
        [
            ("x", "Left/x"),
            ("y", "Left/y"),
            ("x", "Right/x"),
            ("y", "Right/y"),
            ("x", "State/Named/x"),
        ],
    );
    assert_eq!(
        accesses(ReferenceRole::Read),
        [
            ("left.x", "Left/x"),
            ("right.x", "Right/x"),
            ("x", "Left/x"),
            ("y", "Left/y"),
            ("x", "State/Named/x"),
        ],
    );
    assert_eq!(calls(&result, "src/lib.rs").count(), 3);
    assert!(!result.ended);
}

#[test]
fn rust_field_sites_keep_visible_owners_and_refuse_hidden_fields() {
    let source = r"
pub struct Target { pub item: u8 }
pub fn before() -> u8 { { struct Target { item: u16 } } let value = Target { item: 7 }; value.item }
pub fn after() -> u8 { let value = Target { item: 7 }; { struct Target { item: u16 } } value.item }
pub fn clear() -> u8 { let value = Target { item: 7 }; value.item }
pub fn qualified() -> u8 { struct Target { item: u16 } let value = crate::Target { item: 7 }; value.item }
pub fn in_block() -> u16 { struct Local { item: u16 } let value = Local { item: 7 }; value.item }
pub fn sibling_pattern(value: Target) -> u8 { { struct Target { item: u16 } } let Target { item } = value; item }
pub fn valid_pattern(value: Target) -> u8 { let Target { item } = value; item }
mod hidden { pub struct Target { item: u8 } }
pub fn private_pattern(value: hidden::Target) -> u8 { let hidden::Target { item } = value; item }
pub fn private_initializer() -> hidden::Target { hidden::Target { item: 7 } }
pub fn unavailable_pattern(value: foreign::Target) -> u8 { let foreign::Target { item } = value; item }
";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let sites = result
        .occurrences
        .iter()
        .filter(|edge| edge.edge_kind.is_none() && source_of(edge, source) == "item")
        .map(|edge| {
            let physical = edge.target_binding.as_ref().expect("physical field");
            assert_eq!(physical.unit(), edge.binding.unit());
            let byte = |offset| usize::try_from(offset).expect("field offset");
            let range = physical.range();
            (
                edge.binding.range().start(),
                edge.role,
                target(edge)
                    .strip_prefix("rift://symbol/local/rust/fixture/")
                    .expect("package declaration"),
                &source[byte(range.start())..byte(range.end())],
            )
        })
        .collect::<Vec<_>>();
    let position = |statement| {
        let line = source
            .lines()
            .find(|line| line.starts_with(statement))
            .expect("fixture line");
        let start = source.find(line).expect("fixture line start");
        let field = line
            .find("{ item: 7 }")
            .or_else(|| line.find("{ item }"))
            .expect("field label");
        u64::try_from(start + field + 2).expect("fixture offset")
    };
    // A sibling block's type is invisible at the literal and pattern. Keep the
    // physical module field there and the physical local field inside its block.
    let module_field = |statement, role| (position(statement), role, "Target/item", "pub item: u8");
    assert_eq!(
        sites,
        [
            module_field("pub fn before", ReferenceRole::Write),
            module_field("pub fn after", ReferenceRole::Write),
            module_field("pub fn clear", ReferenceRole::Write),
            module_field("pub fn qualified", ReferenceRole::Write),
            (
                position("pub fn in_block"),
                ReferenceRole::Write,
                "in_block/Local/item",
                "item: u16"
            ),
            module_field("pub fn sibling_pattern", ReferenceRole::Read),
            module_field("pub fn valid_pattern", ReferenceRole::Read),
        ],
    );
    // The private-field literal is still withheld. Neither hidden nor unavailable
    // pattern contributes a read to the exact set above.
    assert_eq!(
        result.omitted.get(&rift_protocol::read::EdgeKind::Writes),
        Some(&1)
    );
}

#[test]
fn rust_shadowed_value_and_conditional_module_do_not_reach_decoys() {
    let source = "#[cfg(test)] mod tasks;\n\
                  fn run() {}\n\
                  fn caller(run: fn()) { run(); crate::tasks::run(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source), ("src/tasks.rs", "pub fn run() {}")],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "src/lib.rs").count(), 0);
    assert_eq!(result.unresolved_sites, 2);
}

#[test]
fn test_python_static_receiver_depth_declines_without_blocking_later_reads() {
    let deep = ".again()".repeat(40);
    let source = format!(
        "class Value:\n    count: int\n    def again(self) -> 'Value': return self\ndef deep(value: Value): return value{deep}.count\ndef short(value: Value): return value.again().count\n"
    );
    let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX * 100);
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && target(edge) == "rift://symbol/local/python/main/Value/count"
        })
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 1, "{result:?}");
    assert_eq!(
        reads[0].edge_from.as_ref().map(rift_core::SymbolId::as_str),
        Some("rift://symbol/local/python/main/short")
    );
    assert!(!result.exhausted, "{result:?}");
}

#[test]
fn test_python_static_inherited_fields_keep_assigned_receiver_types() {
    let source = "class Value:\n    count: int\nclass Base:\n    value: object\nclass Middle(Base):\n    def __init__(self): self.value = Value()\nclass Child(Middle):\n    def __init__(self): self.value = Value()\n    def read(self):\n        alias = self.value\n        return self.value.count + alias.count\nclass GrandChild(Child):\n    def read(self): return self.value.count\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .map(target)
        .collect::<Vec<_>>();
    assert!(
        reads.contains(&"rift://symbol/local/python/main/Base/value"),
        "{result:?}"
    );
    assert_eq!(
        reads
            .iter()
            .filter(|target| **target == "rift://symbol/local/python/main/Value/count")
            .count(),
        3,
        "{result:?}"
    );
}

#[test]
fn test_python_static_checked_receivers_reject_shadowing_and_rebinding() {
    let source = "class Value:\n    count: int\ndef valid(value):\n    if not isinstance(value, Value) or disabled:\n        return\n    value.count = 1\ndef changed(value):\n    if not isinstance(value, Value): return\n    value = unknown()\n    value.count = 1\ndef shadowed(value, isinstance):\n    if not isinstance(value, Value): return\n    value.count = 1\ndef nested(value):\n    if not isinstance(value, Value): return\n    def write(): value.count = 1\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let writes = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Write)
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        ["rift://symbol/local/python/main/Value/count"],
        "{result:?}"
    );
}

#[test]
fn test_python_static_alias_cycles_and_function_objects_remain_unresolved() {
    let source = "class Value:\n    count: int\ndef build() -> Value: return Value()\ndef root(value: Value):\n    alias = value\n    alias.count = 1\n    build.count = 2\n    cyclic = cyclic.item\n    cyclic.count = 3\n    first = second\n    second = first\n    first.count = 4\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let writes = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Write)
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        ["rift://symbol/local/python/main/Value/count"],
        "{result:?}"
    );
    assert!(!result.exhausted, "{result:?}");
}

#[test]
fn test_python_static_inherited_fields_keep_the_declared_base_target() {
    let source = "class Base:\n    value: int\nclass Child(Base):\n    def __init__(self): self.value = 1\n    def read(self): return self.value\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let accesses = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.edge_kind.is_none()
                && matches!(edge.role, ReferenceRole::Read | ReferenceRole::Write)
        })
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        accesses, ["rift://symbol/local/python/main/Base/value"; 2],
        "{result:?}"
    );
}

#[test]
fn test_python_static_aliases_and_return_annotations_resolve_members() {
    let source = "class Value:\n    count: int\n    def run(self): pass\ndef build() -> Value | None: return None\ndef root():\n    first = build()\n    second = first\n    second.count = 1\n    second.run()\n    build().count = 2\n    return build().count\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let writes = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Write)
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        writes, ["rift://symbol/local/python/main/Value/count"; 2],
        "{result:?}"
    );
    assert!(
        calls(&result, "main.py")
            .any(|edge| target(edge) == "rift://symbol/local/python/main/Value/run"),
        "{result:?}"
    );
}

#[test]
fn test_python_static_module_aliases_and_instance_class_attributes() {
    let result = resolution(
        "python",
        &[
            (
                "pkg/__init__.py",
                "count = 0\nclass Value:\n    count = 0\n",
            ),
            (
                "main.py",
                "import pkg\nmodule = pkg\ndef root():\n    module.count = 1\n    value = pkg.Value()\n    alias = value\n    return alias.count\ndef nested():\n    class Base:\n        count = 0\n    class Child(Base):\n        count = 1\n    value = Child()\n    return value.count\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Write
                && target(edge) == "rift://symbol/local/python/pkg/count"),
        "{result:?}"
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && target(edge) == "rift://symbol/local/python/pkg/Value/count"),
        "{result:?}"
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && target(edge) == "rift://symbol/local/python/main/nested/Child/count"),
        "{result:?}"
    );
}

#[test]
fn test_python_static_imports_keep_submodules_and_captured_external_exports() {
    let result = resolution(
        "python",
        &[
            (
                "main.py",
                "from pkg import models\nfrom pkg.bridge import External\n",
            ),
            ("pkg/__init__.py", ""),
            ("pkg/models.py", "class Value: pass\n"),
            (
                "pkg/bridge.py",
                "from external import External as External\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    let imported = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.role == ReferenceRole::Import && edge.binding.unit().key().as_str() == "main.py"
        })
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        imported,
        [
            "rift://symbol/local/python/pkg/models",
            "rift://symbol/local/python/pkg/bridge",
            "rift://symbol/local/python/pkg"
        ],
        "{result:?}"
    );
}

#[test]
fn test_python_cross_module_aliases_reuse_the_class_terminal() {
    use std::cell::Cell;

    let work = [2, 12].map(|links| {
        let mut modules = vec![
            ("sample/__init__.py".to_owned(), String::new()),
            (
                "sample/stage_00.py".to_owned(),
                "class Entry:\n    def visit(self): pass\nAlias = Entry\n".to_owned(),
            ),
        ];
        for stage in 1..=links {
            let previous = stage - 1;
            modules.push((
                format!("sample/stage_{stage:02}.py"),
                format!(
                    "from .stage_{previous:02} import Alias as Previous\nAlias = Previous\ndef run():\n    value = Previous()\n    value.visit()\n"
                ),
            ));
        }
        let final_path = &modules.last().expect("last module").0;
        let sources = modules
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect::<Vec<_>>();
        let used = Cell::new(0);
        let result = resolution_by(
            "python",
            &sources,
            &[],
            1_000_000,
            &|graph, documents, input, budget| {
                super::resolve_by(graph, (documents, &[]), input, budget, |index, work, resolution| {
                    const LOOKUP_WORK: usize = 4096;
                    let file = index.files.get(final_path.as_str()).expect("final module");
                    let call = file
                        .facts
                        .calls()
                        .iter()
                        .find(|call| file.text(call.callee_range) == Some("Previous"))
                        .expect("constructor call");
                    let mut probe = super::Work::new(LOOKUP_WORK);
                    let target = index
                        .edge_python_call(file, call, &mut probe)
                        .expect("constructor target");
                    assert_eq!(target.symbol.name, "Entry");
                    used.set(LOOKUP_WORK - probe.budget.remaining());
                    index.resolve_files(super::FileWork::CAPTURED, work, resolution);
                })
            },
        );
        assert!(!result.exhausted);
        if links == 2 {
            let fallback = resolution("python", &sources, &[], WORK_MAX);
            assert!(!fallback.exhausted);
            assert_eq!(labelled(&result), labelled(&fallback));
        }
        assert_eq!(
            calls(&result, final_path).map(target).collect::<Vec<_>>(),
            [
                "rift://symbol/local/python/sample/stage_00/Entry",
                "rift://symbol/local/python/sample/stage_00/Entry/visit",
            ],
            "{result:?}"
        );
        used.get()
    });
    assert!(work[0] > 0);
    assert!(work[1] <= 2 * work[0], "{work:?}");
}

#[test]
fn test_python_static_reexports_keep_module_import_owners() {
    let source = "from pkg import Value\n";
    let result = resolution(
        "python",
        &[
            ("pkg/__init__.py", "from .models import Value as Value\n"),
            ("pkg/models.py", "class Value: pass\n"),
            ("main.py", source),
        ],
        &[],
        WORK_MAX,
    );
    let imports = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Import)
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 4, "{result:?}");
    let written = imports
        .iter()
        .find(|edge| {
            edge.binding.unit().key().as_str() == "main.py"
                && target(edge) == "rift://symbol/local/python/pkg"
        })
        .expect("written package import");
    assert_eq!(source_of(written, source), "pkg");
    for (unit, owner) in [("main.py", "main"), ("pkg/__init__.py", "pkg")] {
        let edge = imports
            .iter()
            .find(|edge| {
                edge.binding.unit().key().as_str() == unit
                    && target(edge) == "rift://symbol/local/python/pkg/models/Value"
            })
            .expect("terminal value import");
        assert_eq!(
            edge.edge_from.as_ref().expect("import owner").as_str(),
            format!("rift://symbol/local/python/{owner}")
        );
    }
}

#[test]
fn test_python_static_super_calls_and_typed_parameters() {
    let source = "class Base:\n    def __init__(self): pass\n    def run(self): pass\nclass Child(Base):\n    def __init__(self): super().__init__()\n    @classmethod\n    def build(cls): return cls()\n    @staticmethod\n    def invoke(value: Base): value.run()\ndef root(value: 'Base'):\n    value.run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        [
            "rift://symbol/local/python/main/Base/__init__",
            "rift://symbol/local/python/main/Child",
            "rift://symbol/local/python/main/Base/run",
            "rift://symbol/local/python/main/Base/run",
        ],
        "{result:?}"
    );
}

#[test]
fn test_python_static_annotations_keep_values_and_reject_ambiguous_receivers() {
    let source = "class Value:\n    count: int = 0\n    def run(self): pass\nclass Owner:\n    def __init__(self):\n        self.value: Value | None = None\n    def run(self):\n        self.value.run()\n        self.value.count += 1\ncurrent: Value\ndef root(value: Value | Owner, cyclic: cyclic):\n    value.run()\n    cyclic.run()\n    return current.count\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/Value/run"],
        "{result:?}"
    );
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .map(target)
        .collect::<Vec<_>>();
    assert!(
        reads.contains(&"rift://symbol/local/python/main/Owner/value"),
        "{reads:?}"
    );
    assert_eq!(
        reads
            .iter()
            .filter(|target| **target == "rift://symbol/local/python/main/Value/count")
            .count(),
        2,
        "{reads:?}"
    );
}

#[test]
fn test_python_static_super_rejects_shadowed_and_static_calls() {
    let source = "class Base:\n    def run(self): pass\nclass Child(Base):\n    @staticmethod\n    def invalid(): super().run()\n    def shadowed(self, super): super().run()\n    def explicit(self): super(Child, self).run()\n    def rebound(self): self = None; super().run()\n    def valid(self): super( ).run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/Base/run"],
        "{result:?}"
    );
}

#[test]
fn test_python_static_module_chains_and_decorated_calls_keep_their_owners() {
    let source = "import pkg.handlers\nfrom pkg import leaf\nfirst = leaf()\nsecond = pkg.handlers.leaf()\nclass Owner:\n    value = leaf()\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("pkg/__init__.py", "from .handlers import leaf as leaf\n"),
            ("pkg/handlers.py", "@decorator\ndef leaf(): pass\n"),
        ],
        &[],
        WORK_MAX,
    );
    let calls = calls(&result, "main.py").collect::<Vec<_>>();
    assert_eq!(calls.len(), 3, "{result:?}");
    for (call, owner) in calls.into_iter().zip(["main", "main", "main/Owner"]) {
        assert_eq!(target(call), "rift://symbol/local/python/pkg/handlers/leaf");
        assert_eq!(
            call.edge_from.as_ref().map(rift_core::SymbolId::as_str),
            Some(format!("rift://symbol/local/python/{owner}").as_str())
        );
    }
}

#[test]
fn test_python_static_constructor_bindings_follow_named_members_only() {
    let source = "class Value:\n    def run(self): pass\nclass Owner:\n    def __init__(self): self.value = Value()\n    def run(self): self.value.run()\ndef factory(): return Value()\ndef root():\n    value.run()\n    value = Value()\n    value.run()\n    unknown = factory()\n    unknown.run()\n    first, second = Value()\n    first.run()\n    second.run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let methods = calls(&result, "main.py")
        .filter(|call| target(call).ends_with("/run"))
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "rift://symbol/local/python/main/Value/run",
            "rift://symbol/local/python/main/Value/run"
        ],
        "{result:?}"
    );
}

#[test]
fn test_python_calls_in_local_initializers_keep_the_enclosing_callable() {
    let source = "def leaf(): return 1\ndef root():\n    value: int = leaf()\n    def nested():\n        other = leaf()\n    return leaf()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let callers = calls(&result, "main.py")
        .map(|call| call.edge_from.as_ref().map(rift_core::SymbolId::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        callers,
        [
            Some("rift://symbol/local/python/main/root"),
            Some("rift://symbol/local/python/main/root/nested"),
            Some("rift://symbol/local/python/main/root"),
        ]
    );
}

#[test]
fn test_python_static_calls_resolve_receivers_constructors_and_reexports() {
    let source = "from pkg import Derived\nimport pkg.models as models\ndef root():\n    Derived()\n    models.Derived()\n    Derived.create()\nclass Child(Derived):\n    def run(self):\n        self.base()\n        self.run()\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            (
                "pkg/__init__.py",
                "from .models import Derived as Derived\n",
            ),
            (
                "pkg/models.py",
                "class Base(External):\n    def base(self): pass\nclass Derived(Base):\n    @staticmethod\n    def create(): pass\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.py").count(), 5, "{result:?}");
}

#[test]
fn test_python_static_contains_accesses_overrides_and_type_usage() {
    use rift_protocol::read::EdgeKind;
    let source = "import models\nclass Base:\n    value: int = 0\n    def run(self): pass\nclass Child(Base):\n    def run(self, value: models.Value) -> models.Value:\n        self.value = value\n        self.value += 1\n        return models.current\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("models.py", "class Value: pass\ncurrent: Value\n"),
        ],
        &[],
        WORK_MAX,
    );
    for (kind, minimum) in [
        (EdgeKind::Contains, 7),
        (EdgeKind::Reads, 2),
        (EdgeKind::Writes, 2),
        (EdgeKind::Overrides, 1),
        (EdgeKind::HasType, 3),
        (EdgeKind::Imports, 1),
    ] {
        let count = result
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.edge_kind.unwrap_or(match occurrence.role {
                    ReferenceRole::Read => EdgeKind::Reads,
                    ReferenceRole::Write => EdgeKind::Writes,
                    ReferenceRole::Type => EdgeKind::HasType,
                    ReferenceRole::Import => EdgeKind::Imports,
                    _ => EdgeKind::References,
                }) == kind
            })
            .count();
        assert!(count >= minimum, "{kind:?}: {count}; {result:?}");
    }
}

#[test]
fn test_python_static_bindings_keep_lexical_scopes_and_class_order() {
    let source = "def leaf(): pass\ndef run():\n    first, second = (1, 2)\n    values = [leaf() for item in []]\n    leaf()\nclass Root:\n    def run(self): pass\nclass Left(Root): pass\nclass Right(Root):\n    def run(self): pass\nclass Child(Left, Right):\n    def run(  # type: ignore\n        self): self.run(); self.other()\n    def other(self): leaf()\n    @staticmethod\n    def static(self): self.run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let targets = calls(&result, "main.py").map(target).collect::<Vec<_>>();
    assert_eq!(
        targets,
        [
            "rift://symbol/local/python/main/leaf",
            "rift://symbol/local/python/main/leaf",
            "rift://symbol/local/python/main/Child/run",
            "rift://symbol/local/python/main/Child/other",
            "rift://symbol/local/python/main/leaf",
        ],
        "{result:?}"
    );
    let overridden = result
        .occurrences
        .iter()
        .filter(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Overrides))
        .map(target)
        .collect::<Vec<_>>();
    assert!(
        overridden.contains(&"rift://symbol/local/python/main/Right/run"),
        "{overridden:?}"
    );
}

#[test]
fn test_python_static_namespace_packages_and_forward_type_arguments() {
    let source = "import pkg.models\nfrom pkg import models\nclass Child(pkg.models.Base[models.Value]):\n    def run(self, value: list['pkg.models.Value']) -> 'models.Value': pass\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("pkg/models.py", "class Base: pass\nclass Value: pass\n"),
        ],
        &[],
        WORK_MAX,
    );
    let types = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Type && edge.edge_kind.is_none())
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        types
            .iter()
            .filter(|id| **id == "rift://symbol/local/python/pkg/models/Value")
            .count(),
        3,
        "{result:?}"
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Extends)),
        "{result:?}"
    );
}

#[test]
fn test_python_global_and_nonlocal_names_do_not_block_other_bindings() {
    let source = "from jobs import leaf\ndef root():\n    active = False\n    def finish():\n        nonlocal active\n        active = True\n        leaf()\n    def send():\n        nonlocal active\n        finish()\n    def global_call():\n        global leaf\n        leaf()\n";
    let result = resolution(
        "python",
        &[("main.py", source), ("jobs.py", "def leaf(): pass\n")],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.py").count(), 3, "{result:?}");
}

#[test]
fn test_python_static_incomplete_bases_keep_only_the_first_base_member() {
    let source = "class Root:\n    def inherited(self): pass\nclass Left(Root):\n    def own(self): pass\nclass Right(External): pass\nclass Child(Left, Right):\n    def run(self):\n        self.own()\n        self.inherited()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/Left/own"]
    );
    assert!(!result.exhausted);
    let exhausted = resolution("python", &[("main.py", source)], &[], 1);
    assert!(exhausted.exhausted);
    assert!(exhausted.occurrences.is_empty());
}

#[test]
fn test_rust_grouped_imports_and_known_receivers_resolve_calls() {
    let source = "mod jobs; use crate::jobs::{Task, leaf as run}; fn root(task: &Task) { run(); Task::new(); task.execute(); }";
    let result = resolution(
        "rust",
        &[
            ("src/lib.rs", source),
            (
                "src/jobs.rs",
                "pub struct Task; pub fn leaf() {} impl Task { pub fn new() -> Self { Self } pub fn execute(&self) {} }",
            ),
        ],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs")
        .map(|edge| source_of(edge, source))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        ["run()", "Task::new()", "task.execute()"],
        "{result:?}"
    );
}

#[test]
fn test_rust_generic_impl_self_and_trait_calls_resolve() {
    let source = "trait Run { fn run(&self); } struct Task<T> { value: T } impl<T> Task<T> { fn new() {} fn execute(&self) { Self::new(); self.run(); } } impl<T> Run for Task<T> { fn run(&self) { self.execute(); } }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs")
        .map(|edge| source_of(edge, source))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        ["Self::new()", "self.run()", "self.execute()"],
        "{result:?}"
    );
}

#[test]
fn test_rust_reference_aliases_probe_their_defining_type() {
    let manifest = [("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")];
    for (prefix, alias_file, generic, alias, trait_call) in [
        (
            "mod types; use crate::types::Alias;",
            "pub type Alias = Vec<u8>;",
            "",
            "Alias",
            false,
        ),
        ("pub type Alias<T> = Vec<T>;", "", "<T>", "Alias<T>", false),
        (
            "mod types; use crate::types::Alias;",
            "use std::vec::Vec as List; pub type Alias<T> = List<T>;",
            "<T>",
            "Alias<T>",
            false,
        ),
        (
            "mod types; use crate::types::Alias;",
            "mod inner { pub type List<T> = Vec<T>; } pub type Alias<T> = inner::List<T>;",
            "<T>",
            "Alias<T>",
            false,
        ),
        (
            "mod types; use crate::types::Alias;",
            "pub struct Vec<T>(pub T); pub type Alias<T> = Vec<T>;",
            "<T>",
            "Alias<T>",
            true,
        ),
        ("pub type Alias<Vec> = Vec;", "", "<T>", "Alias<T>", true),
    ] {
        let source = format!(
            "{prefix}\npub trait Q {{ fn len(&self) -> usize; fn probe(self) -> usize; }}\nimpl{generic} Q for &{alias} {{ fn len(&self) -> usize {{ 99 }} fn probe(self) -> usize {{ self.len() }} }}\n"
        );
        let mut files = vec![("src/lib.rs", source.as_str())];
        if !alias_file.is_empty() {
            files.push(("src/types.rs", alias_file));
        }
        let result = resolution("rust", &files, &manifest, WORK_MAX);
        assert!(!result.exhausted, "{source}");
        assert_eq!(
            calls(&result, "src/lib.rs").any(|edge| source_of(edge, &source) == "self.len()"),
            trait_call,
            "{source}\n{alias_file}"
        );
        assert_eq!(
            result.occurrences.iter().any(|edge| {
                edge.role == ReferenceRole::Read && source_of(edge, &source) == "self.len"
            }),
            trait_call,
            "{source}\n{alias_file}"
        );
        let site = u64::try_from(source.find("self.len()").expect("call site")).expect("offset");
        assert_eq!(
            result.edge_calls.values().any(|coverage| {
                !coverage.unavailable && coverage.unresolved.iter().any(|range| range.start == site)
            }),
            !trait_call,
            "{source}\n{alias_file}"
        );
    }
}

#[test]
fn test_rust_nested_use_groups_keep_module_aliases_and_globs() {
    for source in [
        "mod jobs; use crate::jobs::{self as tasks, *}; fn root() { leaf(); tasks::leaf(); }",
        "mod jobs; use jobs::{self as tasks, *}; fn root() { leaf(); tasks::leaf(); }",
    ] {
        let result = resolution(
            "rust",
            &[("src/lib.rs", source), ("src/jobs.rs", "pub fn leaf() {}")],
            &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
            WORK_MAX,
        );
        let found = calls(&result, "src/lib.rs")
            .map(|edge| source_of(edge, source))
            .collect::<Vec<_>>();
        assert_eq!(found, ["leaf()", "tasks::leaf()"], "{result:?}");
    }
}

#[test]
fn test_rust_tuple_bindings_keep_unrelated_calls() {
    let source = "fn leaf() {} fn root() { let (left, right) = (0, 1); leaf(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "src/lib.rs").count(), 1, "{result:?}");
}

#[test]
fn test_typescript_declared_receiver_fields_follow_aliases_without_cycles() {
    let source = "interface Leaf { run(): void; value: number; } interface Outer { inner: Leaf; } type Alias = Outer; type Cycle = Cycle; function root(input: Alias, cycle: Cycle) { input.inner.run(); cycle.run(); return input.inner.value; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/typescript/.%2Fmain.ts/Leaf/run"]
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Read && target(edge).ends_with("/Leaf/value"))
    );
}

#[test]
fn test_typescript_deferred_lookup_keeps_immediately_invoked_functions_unresolved() {
    let source = "(() => leaf())(); const leaf = () => {}; function deferred() { future(); } const future = () => {};";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/typescript/.%2Fmain.ts/future"]
    );
}

#[test]
fn test_typescript_lexical_accesses_keep_patterns_and_loop_sources() {
    let source = "let value = 0; let other = []; function root({value: local = value}) { ({value} = other); for (const item of other) { value += item; } return {value}; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let count = |name: &str, role| {
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == role && target(edge).ends_with(name))
            .count()
    };
    assert_eq!(count("/value", ReferenceRole::Read), 3);
    assert_eq!(count("/value", ReferenceRole::Write), 2);
    assert_eq!(count("/other", ReferenceRole::Read), 2);
}

#[test]
fn test_typescript_declared_receiver_types_reject_qualified_alias_cycles() {
    let source = "type Broken = Broken.Member; function root(input: Broken) { input.run(); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], 256);
    assert_eq!(calls(&result, "main.ts").count(), 0);
    assert!(!result.exhausted);
}

#[test]
fn test_typescript_deferred_lookup_keeps_decorator_calls_immediate() {
    let source = "class Box { @later() run() {} } const later = () => {}; function root(arg = future()) { later(); } const future = () => {};";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts")
            .filter(|edge| target(edge).ends_with("/later"))
            .count(),
        1
    );
    assert_eq!(
        calls(&result, "main.ts")
            .filter(|edge| target(edge).ends_with("/future"))
            .count(),
        1
    );
}

#[test]
fn test_typescript_loop_assignment_records_writes_and_invalidates_calls() {
    let source = "let value = 0; const values = []; function leaf() {} function root() { for (value of values) {} for (leaf of values) { leaf(); } }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Write && target(edge).ends_with("/value"))
            .count(),
        1
    );
    assert_eq!(
        calls(&result, "main.ts")
            .filter(|edge| target(edge).ends_with("/leaf"))
            .count(),
        0
    );
}

#[test]
fn test_typescript_declared_annotations_keep_partial_extraction() {
    for broken in [
        "type Broken = ;",
        "function bad(input: ) {}",
        "interface Broken { value: }",
    ] {
        let source = format!("{broken} function leaf() {{}} export function root() {{ leaf(); }}");
        let result = resolution("typescript", &[("main.ts", &source)], &[], WORK_MAX);
        assert_eq!(
            calls(&result, "main.ts")
                .filter(|edge| target(edge).ends_with("/leaf"))
                .count(),
            1,
            "{broken}"
        );
    }
}

#[test]
fn test_typescript_declared_receiver_types_allow_annotation_comments() {
    let source =
        "interface Box { run(): void; } function root(input: /* type */ Box) { input.run(); }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.ts").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/typescript/.%2Fmain.ts/Box/run"]
    );
}

#[test]
fn test_typescript_qualified_annotation_names_are_not_value_accesses() {
    let source = "namespace Types { export interface Value {} } function root(input: Types.Value) { return input; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Read && target(edge).ends_with("/Types"))
            .count(),
        0
    );
    assert_eq!(
        result
            .occurrences
            .iter()
            .filter(
                |edge| edge.role == ReferenceRole::Type && target(edge).ends_with("/Types/Value")
            )
            .count(),
        1
    );
}

#[test]
fn test_typescript_declaration_type_use_selects_the_physical_merged_alias() {
    let source = "export declare namespace Shape { export type Nested = { item?: Shape }; } export type Shape = { label: string }; declare namespace Types { export interface Member {} } export type Qualified = Types.Member;";
    let path = "main.d.ts";
    let result = resolution("typescript", &[(path, source)], &[], WORK_MAX);
    let reference_start = u64::try_from(source.find("?: Shape").expect("type reference") + 3)
        .expect("reference start");
    let mut occurrences = result.occurrences.iter().filter(|edge| {
        edge.role == ReferenceRole::Type && edge.binding.range().start() == reference_start
    });
    let edge = occurrences.next().expect("merged alias use");
    assert_eq!(source_of(edge, source), "Shape");
    assert_eq!(edge.binding.unit().key().as_str(), path);
    assert_eq!(
        target(edge),
        "rift://symbol/local/typescript/.%2Fmain.d.ts/Shape"
    );
    let physical = edge.target_binding.as_ref().expect("physical alias");
    assert_eq!(physical.unit(), edge.binding.unit());
    let declaration = "type Shape = { label: string };";
    let alias_start =
        u64::try_from(source.find(declaration).expect("alias declaration")).expect("alias start");
    let alias_length = u64::try_from(declaration.len()).expect("alias length");
    assert_eq!(physical.range().start(), alias_start);
    assert_eq!(physical.range().end(), alias_start + alias_length);
    assert!(occurrences.next().is_none());
    assert!(result.occurrences.iter().any(|edge| {
        edge.role == ReferenceRole::Type
            && source_of(edge, source) == "Types.Member"
            && target(edge).ends_with("/Types/Member")
    }));
}

#[test]
fn test_typescript_merged_alias_refuses_competing_and_wrong_space_bindings() {
    let fixtures = [
        "declare namespace Shape { export type Nested = { item?: Shape }; }",
        "export declare namespace Shape { export type Nested = { item?: Shape }; } export type Shape = number; export type Shape = string;",
        "export declare namespace Shape { export type Nested<Shape> = { item?: Shape }; } export type Shape = number;",
        "declare namespace Shape { export type Nested = { item?: Shape }; } export type Shape = number;",
        "declare namespace Shapes { export type Shape = number; } declare namespace Shape { export type Nested = { item?: Shape }; }",
    ];
    for path in ["main.d.ts", "main.ts"] {
        let logical = format!("rift://symbol/local/typescript/.%2F{path}/Shape");
        for source in fixtures {
            let result = resolution("typescript", &[(path, source)], &[], WORK_MAX);
            assert!(
                !result.occurrences.iter().any(|edge| {
                    edge.role == ReferenceRole::Type
                        && source_of(edge, source) == "Shape"
                        && target(edge) == logical
                }),
                "{path} {source}: {result:?}"
            );
        }
    }
}

#[test]
fn test_typescript_source_type_use_selects_the_physical_merged_alias() {
    let source = "export namespace Result { export function make() {} }
export type Result<T> = { data: T };
export type One = Result<number>;
export type Runtime = typeof Result;
export type Shadow<Result> = Result;
export namespace Other { export type Result<T> = T; export type Two = Result<string>; }";
    let result = resolution("typescript", &[("main.ts", source)], &[], WORK_MAX);
    let logical = "rift://symbol/local/typescript/.%2Fmain.ts/Result";
    let annotation = source.find("Result<number>").expect("type annotation");
    let mut alias_types = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Type && target(edge) == logical);
    let edge = alias_types.next().expect("merged type alias");
    assert!(alias_types.next().is_none());
    assert_eq!(source_of(edge, source), "Result");
    assert_eq!(
        edge.binding.range().start(),
        u64::try_from(annotation).expect("type start")
    );
    let physical = edge.target_binding.as_ref().expect("physical type alias");
    assert_eq!(physical.unit(), edge.binding.unit());
    let declaration = "type Result<T> = { data: T };";
    let alias_start = source.find(declaration).expect("type declaration");
    assert_eq!(
        physical.range().start(),
        u64::try_from(alias_start).expect("alias start")
    );
    assert_eq!(
        physical.range().end(),
        u64::try_from(alias_start + declaration.len()).expect("alias end")
    );
    assert!(result.occurrences.iter().any(|edge| {
        edge.role == ReferenceRole::Type
            && source_of(edge, source) == "Result"
            && target(edge).ends_with(".%2Fmain.ts/Other/Result")
    }));
}

#[test]
fn test_rust_trait_default_and_explicit_bounds_keep_source_and_identity() {
    let source = "pub trait Count { fn remaining(&self) -> usize; fn get(&self) -> usize { self.remaining() } } pub fn generic<T: Count>(item: &T) -> usize { item.remaining() } pub fn bounded<T>(item: &T) -> usize where T: Count { item.remaining() }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let resolved = calls(&result, "src/lib.rs")
        .map(|edge| (source_of(edge, source), target(edge)))
        .collect::<Vec<_>>();
    assert_eq!(
        resolved,
        [
            (
                "self.remaining()",
                "rift://symbol/local/rust/fixture/Count/remaining"
            ),
            (
                "item.remaining()",
                "rift://symbol/local/rust/fixture/Count/remaining"
            ),
            (
                "item.remaining()",
                "rift://symbol/local/rust/fixture/Count/remaining"
            )
        ]
    );
}

#[test]
fn test_rust_bound_lookup_refuses_unknown_and_competing_traits() {
    let source = "trait Count { fn remaining(&self); } trait Other { fn remaining(&self); } trait Marker {} fn unique<T: Marker + Count>(item: &T) { item.remaining(); } fn competing<T: Count + Other>(item: &T) { item.remaining(); } fn unknown<T: Count + missing::Other>(item: &T) { item.remaining(); } fn higher<T: Count + for<'a> missing::Other<'a>>(item: &T) { item.remaining(); } fn shadow<T: Count>(item: &T) { let item = 1; item.remaining(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let resolved = calls(&result, "src/lib.rs").collect::<Vec<_>>();
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        target(resolved[0]),
        "rift://symbol/local/rust/fixture/Count/remaining"
    );
    assert_eq!(
        resolved[0].binding.range().start(),
        u64::try_from(source.find("item.remaining()").expect("unique receiver")).expect("offset")
    );
}

#[test]
fn test_rust_default_feature_conditions_preserve_exact_call_source_and_target() {
    let source = "#[cfg(feature=\"std\")] fn leaf() {} #[cfg(feature=\"off\")] fn inactive() { leaf(); } fn root() { leaf(); } #[cfg(unknown)] fn mystery() {} fn uncertain() { mystery(); }";
    let manifest =
        "[package]\nname='fixture'\nversion='0.1.0'\n[features]\ndefault=['std']\nstd=[]\noff=[]\n";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", manifest)],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs").collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert_eq!(source_of(found[0], source), "leaf()");
    assert_eq!(target(found[0]), "rift://symbol/local/rust/fixture/leaf");
    assert_eq!(
        found[0].binding.range().start(),
        u64::try_from(source.rfind("leaf()").expect("active call")).expect("offset")
    );
}

#[test]
fn test_rust_pattern_field_attributes_control_just_the_field_binding() {
    let manifest = [("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")];
    for (condition, calls_leaf) in [("all()", false), ("any()", true)] {
        let source = format!(
            "pub fn cfg() {{}} pub fn leaf() {{}} pub struct Held {{ leaf: fn() }} pub fn run(held: Held) {{ let Held {{ #[cfg({condition})] leaf, .. }} = held; cfg(); leaf(); }}"
        );
        let result = resolution("rust", &[("src/lib.rs", &source)], &manifest, WORK_MAX);
        assert!(!result.exhausted, "{source}");
        let found = calls(&result, "src/lib.rs").map(target).collect::<Vec<_>>();
        assert_eq!(found.len(), 1 + usize::from(calls_leaf), "{source}");
        assert!(
            found.iter().any(|target| target.ends_with("/fixture/cfg")),
            "{source}"
        );
        assert_eq!(
            found.iter().any(|target| target.ends_with("/fixture/leaf")),
            calls_leaf,
            "{source}"
        );
    }
}

#[test]
fn test_rust_near_value_declarations_hide_outer_functions() {
    for (body, expected) in [
        ("pub fn run() { static leaf: fn() = other; leaf(); }", None),
        ("pub fn run() { const leaf: fn() = other; leaf(); }", None),
        (
            "pub fn run() { struct leaf(u8); leaf(1); }",
            Some("run/leaf"),
        ),
        (
            "pub fn run() { unsafe extern \"C\" { fn leaf(); } unsafe { leaf() }; }",
            Some("run/leaf"),
        ),
        (
            "pub enum E { leaf(u8) } pub fn run() { use E::*; leaf(1); }",
            Some("E/leaf"),
        ),
        (
            "pub mod m { use super::*; pub static leaf: fn(u8) = |_| (); pub fn run() { leaf(1); } }",
            None,
        ),
        (
            "pub mod m { use super::*; pub struct leaf(pub u8); pub fn run() { leaf(1); } }",
            Some("m/leaf"),
        ),
        (
            "pub mod m { use super::*; unsafe extern \"C\" { pub fn leaf(); } pub fn run() { unsafe { leaf() }; } }",
            Some("m/leaf"),
        ),
        ("pub fn run() { fn leaf() {} leaf(); }", Some("run/leaf")),
        (
            "pub fn run() { const leaf: fn() = other; crate::leaf(); }",
            Some("leaf"),
        ),
        (
            "pub mod m { use super::*; pub fn run() { leaf(); } }",
            Some("leaf"),
        ),
        (
            "pub fn run() { struct leaf { field: u8 } leaf(); }",
            Some("leaf"),
        ),
        (
            "pub struct Item; impl Item { const leaf: fn() = other; pub fn run() { leaf(); } }",
            Some("leaf"),
        ),
        (
            "pub enum E { leaf(u8) } use E::*; pub fn run() { leaf(); }",
            Some("leaf"),
        ),
    ] {
        let source = format!("pub fn leaf() {{}}\npub fn other() {{}}\n{body}");
        let manifest = [("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")];
        let result = resolution("rust", &[("src/lib.rs", &source)], &manifest, WORK_MAX);
        assert!(!result.exhausted, "{body}");
        let expected = expected.map(|name| format!("rift://symbol/local/rust/fixture/{name}"));
        let found = calls(&result, "src/lib.rs").map(target).collect::<Vec<_>>();
        assert_eq!(
            found,
            expected.as_deref().into_iter().collect::<Vec<_>>(),
            "{body}"
        );
    }
}

#[test]
fn test_rust_declarative_macro_bindings_have_statement_scope() {
    const BIND: &str = "macro_rules! bind { ($name:ident) => { let $name = other; }; }";
    const INSPECT: &str = "macro_rules! bind { ($name:ident) => { let local = $name; }; }";
    const CALL: &str = "/* call */ ";
    for body in [
        format!("{BIND} bind!(leaf); leaf();"),
        format!("{CALL}leaf(); {BIND} bind!(leaf); leaf();"),
        format!("{BIND} {{ bind!(leaf); leaf(); }} {CALL}leaf();"),
        format!("{BIND} bind!(leaf); {CALL}crate::leaf();"),
        format!("{BIND} bind!(other); {CALL}leaf();"),
        format!("{INSPECT} bind!(leaf); {CALL}leaf();"),
        format!("{BIND} {INSPECT} bind!(leaf); {CALL}leaf();"),
        format!("{INSPECT} #[cfg(any())] {BIND} bind!(leaf); {CALL}leaf();"),
    ] {
        let source = format!("pub fn leaf() {{}}\npub fn other() {{}}\npub fn run() {{ {body} }}");
        let manifest = [("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")];
        let result = resolution("rust", &[("src/lib.rs", &source)], &manifest, WORK_MAX);
        assert!(!result.exhausted, "{body}");
        let expected = source
            .find(CALL)
            .map(|marker| u64::try_from(marker + CALL.len()).expect("offset"));
        let found = calls(&result, "src/lib.rs").collect::<Vec<_>>();
        assert_eq!(found.len(), 1 + usize::from(expected.is_some()), "{body}");
        let ordinary = found
            .iter()
            .filter(|edge| target(edge).ends_with("/fixture/leaf"))
            .map(|edge| edge.callee_range.start)
            .collect::<Vec<_>>();
        assert_eq!(ordinary, expected.into_iter().collect::<Vec<_>>(), "{body}");

        let binding = found
            .into_iter()
            .find(|edge| target(edge).ends_with("/fixture/run/bind"))
            .expect("authored macro call");
        let invocation = if body.contains("bind!(other)") {
            "bind!(other)"
        } else {
            "bind!(leaf)"
        };
        let invocation_start = source.find(invocation).expect("macro invocation");
        assert_eq!(source_of(binding, &source), invocation, "{body}");
        assert_eq!(
            binding.binding.range().start(),
            u64::try_from(invocation_start).expect("invocation offset"),
            "{body}",
        );
        let definition_start = source
            .find(INSPECT)
            .or_else(|| source.find(BIND))
            .expect("active macro definition");
        assert_eq!(
            binding
                .target_binding
                .as_ref()
                .expect("macro target")
                .range()
                .start(),
            u64::try_from(definition_start).expect("definition offset"),
            "{body}",
        );
    }
}

#[test]
fn test_rust_unknown_import_does_not_establish_a_receiver_type() {
    let source =
        "mod known; #[cfg(unknown)] use crate::known::Item; fn root(item: &Item) { item.read(); }";
    let result = resolution(
        "rust",
        &[
            ("src/lib.rs", source),
            (
                "src/known.rs",
                "pub struct Item; impl Item { pub fn read(&self) {} }",
            ),
        ],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "src/lib.rs").count(), 0);
}

#[test]
fn test_rust_reexport_chain_calls_keep_authored_trait_identity() {
    let source = "pub mod inner; pub use crate::inner::Count; pub fn root<T: Count>(item: &T) { item.read(); }";
    let result = resolution(
        "rust",
        &[
            ("src/lib.rs", source),
            (
                "src/inner/mod.rs",
                "mod traits; pub use self::traits::Count;",
            ),
            ("src/inner/traits.rs", "pub trait Count { fn read(&self); }"),
        ],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs").collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert_eq!(source_of(found[0], source), "item.read()");
    assert_eq!(
        target(found[0]),
        "rift://symbol/local/rust/fixture/inner/traits/Count/read"
    );
}

#[test]
fn test_rust_public_type_reexport_keeps_public_method_access() {
    let source = "pub mod inner; pub use crate::inner::Item; pub fn root() { Item::new(); }";
    let result = resolution(
        "rust",
        &[
            ("src/lib.rs", source),
            ("src/inner/mod.rs", "mod types; pub use self::types::Item;"),
            (
                "src/inner/types.rs",
                "pub struct Item; impl Item { pub fn new() -> Self { Self } }",
            ),
        ],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs").collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert_eq!(source_of(found[0], source), "Item::new()");
    assert_eq!(
        target(found[0]),
        "rift://symbol/local/rust/fixture/inner/types/Item/new"
    );
}

#[test]
fn test_rust_inactive_generic_bounds_do_not_change_outer_receivers() {
    let source = "trait Count { fn read(&self); } trait Other { fn read(&self); } fn root<T: Count>(item: &T) { #[cfg(any())] fn inactive<T: Other>(item: &T) {} fn T() -> OtherItem { loop {} } item.read(); } struct OtherItem; impl OtherItem { fn read(&self) {} }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    assert_eq!(
        calls(&result, "src/lib.rs").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/rust/fixture/Count/read"]
    );
}

#[test]
fn test_rust_local_return_receiver_uses_the_factory_declaration_scope() {
    let source = "mod items; use crate::items::make; fn root() { let item = make(); item.read(); }";
    let result = resolution(
        "rust",
        &[
            ("src/lib.rs", source),
            (
                "src/items.rs",
                "pub struct Item; impl Item { pub fn read(&self) {} } pub fn make() -> Item { Item }",
            ),
        ],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs")
        .map(|edge| (source_of(edge, source), target(edge)))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            ("make()", "rift://symbol/local/rust/fixture/items/make"),
            (
                "item.read()",
                "rift://symbol/local/rust/fixture/items/Item/read"
            )
        ]
    );
}

#[test]
fn test_rust_local_receiver_depth_refuses_without_exhausting_the_pass() {
    use std::fmt::Write as _;
    let mut source =
        "struct Item; impl Item { fn read(&self) {} } fn root(item: &Item) {".to_owned();
    for index in 0..80 {
        let previous = if index == 0 {
            "item".to_owned()
        } else {
            format!("value{}", index - 1)
        };
        write!(source, "let value{index} = {previous};").expect("fixture source");
    }
    source.push_str("value79.read(); }");
    let result = resolution(
        "rust",
        &[("src/lib.rs", &source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "src/lib.rs").count(), 0);
    assert!(result.unresolved_sites > 0);
    assert!(!result.exhausted);
}

#[test]
fn test_rust_alias_of_a_bounded_receiver_keeps_its_type_parameter_scope() {
    let source = "trait Count { fn read(&self); } fn root<T: Count>(item: &T) { let alias = item; alias.read(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs").collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert_eq!(source_of(found[0], source), "alias.read()");
    assert_eq!(
        target(found[0]),
        "rift://symbol/local/rust/fixture/Count/read"
    );
}

#[test]
fn test_rust_unit_self_constructor_keeps_its_authored_owner() {
    let source =
        "struct Item; impl Item { fn read(&self) {} fn root() { let item = Self; item.read(); } }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/lib.rs").collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert_eq!(source_of(found[0], source), "item.read()");
    assert_eq!(
        target(found[0]),
        "rift://symbol/local/rust/fixture/Item/read"
    );
}

#[test]
fn test_rust_non_trait_bound_does_not_supply_a_same_named_method() {
    let source =
        "struct Item; impl Item { fn read(&self) {} } fn root<T: Item>(item: &T) { item.read(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "src/lib.rs").count(), 0);
}

#[test]
fn test_rust_crate_visible_nested_modules_keep_import_targets() {
    let source = "mod provider; mod consumer;";
    let consumer =
        "use crate::provider::nested::Count; fn root<T: Count>(item: &T) { item.read(); }";
    let result = resolution(
        "rust",
        &[
            ("src/lib.rs", source),
            (
                "src/provider.rs",
                "pub(crate) mod nested { pub(crate) trait Count { fn read(&self); } }",
            ),
            ("src/consumer.rs", consumer),
        ],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let found = calls(&result, "src/consumer.rs").collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert_eq!(source_of(found[0], consumer), "item.read()");
    assert_eq!(
        target(found[0]),
        "rift://symbol/local/rust/fixture/provider/nested/Count/read"
    );
}

#[test]
fn test_rust_field_and_method_names_keep_exact_access_targets() {
    let source = "struct Item { value: usize } impl Item { fn value(&self) -> usize { self.value } fn call(&self) { self.value(); } fn invalid(&self) { self.missing; } fn missing(&self) {} }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 2);
    let field = reads
        .iter()
        .find(|edge| {
            source_of(edge, source) == "self.value"
                && edge.binding.range().start()
                    == u64::try_from(source.find("self.value }").expect("field access"))
                        .expect("offset")
        })
        .expect("field read");
    let target = field
        .target_binding
        .as_ref()
        .expect("target binding")
        .range();
    assert!(
        target.start()
            < u64::try_from(source.find("impl Item").expect("implementation")).expect("offset"),
        "field access must target the field declaration: {target:?}"
    );
    assert!(
        reads
            .iter()
            .any(|edge| source_of(edge, source) == "self.value"
                && edge.binding.range().start()
                    == u64::try_from(source.find("self.value();").expect("method call"))
                        .expect("offset"))
    );
}

mod external;

/// Reading the writes of one file's exported members costs in proportion to the file, and a
/// generated file leaves the budget of the others.
#[test]
fn export_member_writes_charge_the_budget_in_proportion_to_their_file() {
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let generated = |exports: usize| {
        let mut source = String::new();
        for index in 0..exports {
            write!(
                source,
                "exports.n{index} = {{ a: 1 }};\nexports.n{index}.a = 2;\n"
            )
            .expect("generated source");
        }
        source
    };
    let main = "function leaf() {} function root() { leaf(); } module.exports = root;";
    let pass = |exports: usize, work_max: usize| {
        let generated = generated(exports);
        let used = AtomicUsize::new(0);
        let result = resolution_by(
            "javascript",
            &[("generated.cjs", generated.as_str()), ("main.cjs", main)],
            &[],
            work_max,
            &|graph, documents, input, work_max| {
                super::resolve_by(
                    graph,
                    (documents, &[]),
                    input,
                    work_max,
                    |index, work, resolution| {
                        used.store(work_max - work.budget.remaining(), Ordering::Relaxed);
                        index.resolve_files(super::FileWork::CAPTURED, work, resolution);
                    },
                )
            },
        );
        (used.load(Ordering::Relaxed), result)
    };
    let (small, _) = pass(100, 10_000_000);
    let (large, result) = pass(400, 10_000_000);
    assert!(small > 400, "each member and binding is charged: {small}");
    assert!(large < small * 6, "{large} is not in proportion to {small}");
    assert_eq!(calls(&result, "main.cjs").count(), 1);
    let (_, tight) = pass(400, large * 8);
    assert_eq!(calls(&tight, "main.cjs").count(), 1);
    assert!(!tight.edge_calls.is_empty());
}
