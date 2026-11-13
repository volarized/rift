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
    assert_eq!(result.unresolved_sites, 2);
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
fn rust_uses_admitted_module_paths_and_excludes_trait_and_conditional_candidates() {
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
