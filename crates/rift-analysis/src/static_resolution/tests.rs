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

use super::{ReferenceRole, StaticOccurrence, StaticResolution, StaticSource, resolve};
use crate::analyzer::namespace::{SelectedFile, prepare_context};
use crate::semantic::PlacedFacts;
use crate::{ExactPackageLimits, NamespaceInput, PackageSource, WorkspaceSemantics};
use rift_core::ProjectPath;
use rift_provider::NormalizedTarget;
use rift_syntax::DocumentPlacement;

const WORK_MAX: usize = 10_000;

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

fn resolution(
    language: &str,
    sources: &[(&str, &str)],
    metadata: &[(&str, &str)],
    work_max: usize,
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
    let placed = selected
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
                .with_logical_declarations(
                    namespaces
                        .mappings
                        .remove(source.path.as_str())
                        .unwrap_or_default(),
                ),
        })
        .collect::<Vec<_>>();
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
    resolve(semantics.graph(), &documents, &input, work_max)
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
