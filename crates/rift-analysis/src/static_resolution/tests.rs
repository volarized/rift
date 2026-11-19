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
        assert_eq!(edges[0].target_binding.unit().key().as_str(), "lib/jobs.ts");
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
    assert_eq!(edges[0].target_binding.unit().key().as_str(), "jobs.py");
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
        resolved[0].target_binding.unit().key().as_str(),
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
        .map(|(path, text)| syntax(path, text))
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
        .map(|(document, (_, source))| SelectedFile {
            path: document.path(),
            source,
            syntax: document.facts(),
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
    let result = resolve(semantics.graph(), &documents, &input, work_max);
    let mut edges = crate::EdgeStore::build(semantics.graph());
    edges.add_static(
        semantics.graph(),
        &result.occurrences,
        &result.external,
        WORK_MAX,
    );
    for occurrence in &result.external {
        let edge = edges
            .incoming(&occurrence.target)
            .iter()
            .find(|edge| edge.occurrence() == &occurrence.binding)
            .expect("external occurrence reaches its target");
        assert_eq!(
            edge.to_protocol().provider,
            rift_protocol::read::EdgeProvider::Static
        );
    }
    result
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
    assert_eq!(calls(&result, "main.mjs").count(), 0);
    assert_eq!(result.unresolved_sites, 3);
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
        call.target_binding.unit().to_string(),
        "rift://source/project/tasks.mjs"
    );
    let range = call.target_binding.range();
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
    assert!(missing.occurrences.is_empty());
}

#[test]
fn javascript_empty_analysis_completes_without_an_unresolved_site() {
    let result = resolution(
        "javascript",
        &[("empty.mjs", "export function idle() {}")],
        &[],
        WORK_MAX,
    );
    assert!(result.occurrences.is_empty());
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
    assert!(result.occurrences.is_empty());
    assert_eq!(result.analyzed_sources, 2);
    assert_eq!(result.unavailable_sources, 1);
    assert_eq!(result.unresolved_sites, 2);
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

mod external;
