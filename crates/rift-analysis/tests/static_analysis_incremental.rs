//! Cold and retained static analysis across captured source and metadata changes.

#![cfg(feature = "collector")]

use std::sync::Arc;

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, IndexedFile, NamespaceInput, PackageAnalysis,
    PackageAnalyzer, PackageDependency, PackageSource, PackageSyntax, WorkspaceSemantics,
};
use rift_core::{
    ContributionOrigin, FileDigest, ProjectPath, ReferenceRole, SourceKind, SourceLocation,
    SourceUnitId, SymbolId,
};
use rift_protocol::canonical::canonical_json;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{Language, PackageIdentity, RelationshipFacet};
use rift_syntax::{SyntaxLimits, SyntaxSource, registry};

fn document(path: &str, source: &str) -> IndexedFile {
    let path = ProjectPath::new(path).expect("fixture path");
    let extension = path.as_str().rsplit_once('.').expect("extension").1;
    let syntax = registry::provider_for_extension(extension)
        .expect("syntax provider")
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax facts");
    IndexedFile::new(
        path,
        Arc::new(source.to_owned()),
        FileDigest::of(source.as_bytes()),
        false,
        syntax,
    )
}

fn workspace(
    files: &[&IndexedFile],
    mode: &str,
    revision: u64,
    previous: Option<&WorkspaceSemantics>,
) -> WorkspaceSemantics {
    let language = Language::from_identity_segment("javascript").expect("JavaScript");
    let sources: Vec<_> = files
        .iter()
        .map(|file| PackageSource::new(file.path(), file.source()))
        .collect();
    let manifest_path = ProjectPath::new("package.json").expect("manifest");
    let manifest = format!(r#"{{"type":"{mode}"}}"#);
    let context = [PackageSource::new(&manifest_path, &manifest)];
    let input = NamespaceInput::project(
        &SymbolOwner::Local,
        &language,
        &sources,
        &context,
        &[],
        ExactPackageLimits::new(16, 32_768),
    )
    .expect("captured namespace");
    WorkspaceSemantics::build_captured_project_facts(
        input,
        files,
        64,
        64,
        revision,
        previous.map(WorkspaceSemantics::graph),
    )
    .expect("captured relationships")
    .semantics
}

#[test]
fn workspace_edits_match_cold_calls_and_keep_held_evidence() {
    let main = document(
        "main.mjs",
        "import { launch } from './barrel.mjs'; export function caller() { launch(); }",
    );
    let barrel = document(
        "barrel.mjs",
        "export { start as launch } from './jobs.mjs';",
    );
    let edited_barrel = document(
        "barrel.mjs",
        "export { start as launch } from './tasks.mjs';",
    );
    let jobs = document("jobs.mjs", "export function start() {}");
    let tasks = document("tasks.mjs", jobs.source());
    let common = document(
        "common.js",
        "const run = require('./job.cjs').start; function caller() { run(); }",
    );
    let job = document("job.cjs", "function start() {} module.exports = { start };");
    let jobs_id =
        SymbolId::new("rift://symbol/local/javascript/.%2Fjobs.mjs/start").expect("target");
    let tasks_id =
        SymbolId::new("rift://symbol/local/javascript/.%2Ftasks.mjs/start").expect("target");
    let common_id =
        SymbolId::new("rift://symbol/local/javascript/.%2Fjob.cjs/start").expect("target");
    let held = workspace(&[&main, &barrel, &jobs, &common, &job], "commonjs", 1, None);
    let mut previous = workspace(&[&main, &barrel, &jobs, &common, &job], "commonjs", 1, None);
    for (revision, files, mode, expected) in [
        (
            2,
            &[&main, &edited_barrel, &jobs, &tasks, &common, &job][..],
            "commonjs",
            [0, 1, 1],
        ),
        (
            3,
            &[&main, &edited_barrel, &tasks, &common, &job][..],
            "commonjs",
            [0, 1, 1],
        ),
        (
            4,
            &[&main, &edited_barrel, &tasks, &common, &job][..],
            "module",
            [0, 1, 0],
        ),
        (
            5,
            &[&main, &edited_barrel, &common, &job][..],
            "module",
            [0, 0, 0],
        ),
    ] {
        let incremental = workspace(files, mode, revision, Some(&previous));
        let cold = workspace(files, mode, revision, None);
        for (target, count) in [&jobs_id, &tasks_id, &common_id].into_iter().zip(expected) {
            assert_eq!(
                incremental.relationships().incoming(target),
                cold.relationships().incoming(target)
            );
            let occurrences: Vec<_> = incremental.static_usages(target).collect();
            assert_eq!(occurrences, cold.static_usages(target).collect::<Vec<_>>());
            assert_eq!(
                occurrences
                    .iter()
                    .filter(|(_, _, _, role)| *role == ReferenceRole::Call)
                    .count(),
                count,
                "revision {revision} target {target}"
            );
        }
        previous = incremental;
    }
    for (target, original, expected_source) in
        [(&jobs_id, &main, "launch"), (&common_id, &common, "run")]
    {
        let (_, range, digest, _) = held
            .static_usages(target)
            .find(|(_, _, _, role)| *role == ReferenceRole::Call)
            .expect("held call");
        let range =
            usize::try_from(range.start).expect("start")..usize::try_from(range.end).expect("end");
        assert_eq!(&original.source()[range], expected_source);
        assert_eq!(digest, FileDigest::of(original.source().as_bytes()));
    }
}

fn package(name: &str) -> PackageIdentity {
    PackageIdentity {
        manager: "cargo".to_owned(),
        registry: "crates.io".to_owned(),
        name: name.to_owned(),
        version: "1.0.0".to_owned(),
    }
}

fn origin(package: PackageIdentity) -> ContributionOrigin {
    ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("package source origin")
}

#[test]
fn dependency_reselection_rebuilds_export_evidence_over_retained_syntax() {
    const LIMITS: ExactPackageLimits = ExactPackageLimits::new(16, 1_048_576);
    let language = Language::from_identity_segment("rust").expect("Rust");
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest");
    let root = ProjectPath::new("src/lib.rs").expect("library");
    let app = package("app");
    let owner = app.owner().expect("app owner");
    let app_origin = origin(app);
    let app_sources = [PackageSource::new(
        &root,
        "pub use selected::start as launch;",
    )];
    let mut capture: Option<PackageSyntax> = None;
    let mut old = None;
    for (revision, name) in [(1, "jobs"), (2, "tasks")] {
        let dependency = package(name);
        let dependency_owner = dependency.owner().expect("dependency owner");
        let unit = SourceUnitId::for_owner(dependency_owner.clone(), root.as_str())
            .expect("dependency source unit");
        let dependency_origin = origin(dependency);
        let dependency_sources = [PackageSource::new(&root, "pub fn start() {}")
            .with_source_unit(&unit, &dependency_origin)
            .expect("original dependency source")];
        let dependency_manifest =
            format!("[package]\nname='{name}'\nversion='1.0.0'\n[lib]\npath='src/lib.rs'\n");
        let dependency_context = [PackageSource::new(&manifest, &dependency_manifest)];
        let dependency_input = ExactPackageInput::new(
            &dependency_owner,
            &language,
            &dependency_origin,
            &dependency_sources,
            LIMITS,
        )
        .expect("dependency input")
        .with_framework_context(&dependency_context, &[])
        .expect("dependency context")
        .with_library(
            &dependency_owner,
            dependency_context[0],
            dependency_sources[0],
        )
        .expect("dependency library");
        let dependency =
            PackageAnalyzer::analyze(dependency_input, revision).expect("dependency publication");
        let app_manifest = format!(
            "[package]\nname='app'\nversion='1.0.0'\n[dependencies.selected]\npackage='{name}'\nversion='=1.0.0'\n"
        );
        let context = [PackageSource::new(&manifest, &app_manifest)];
        let dependencies = [PackageDependency::new(
            "selected",
            context[0],
            dependency.publication(),
            &dependency_sources,
        )];
        let input = ExactPackageInput::new(&owner, &language, &app_origin, &app_sources, LIMITS)
            .expect("app input")
            .with_framework_context(&context, &[])
            .expect("app context")
            .with_dependencies(&dependencies)
            .expect("current exact dependency");
        let incremental = PackageAnalyzer::analyze_with_syntax(input, revision, |source| {
            if let Some(capture) = &capture {
                return Some(capture.clone());
            }
            capture = Some(source.parse().expect("app capture"));
            capture.clone()
        })
        .expect("retained app analysis");
        let cold = PackageAnalyzer::analyze(input, revision).expect("cold app analysis");
        assert_eq!(
            canonical_json(incremental.publication()).expect("retained publication"),
            canonical_json(cold.publication()).expect("cold publication"),
        );
        assert_eq!(
            incremental.syntax_work().reused_files,
            u64::from(revision == 2)
        );
        assert_export_target(&incremental, name);
        if let Some(held) = &old {
            assert_export_target(held, "jobs");
        }
        old = Some(incremental);
    }
}

fn assert_export_target(analysis: &PackageAnalysis, name: &str) {
    let publication = analysis.publication();
    let edges: Vec<_> = publication
        .relationships
        .iter()
        .filter(|edge| edge.facets.contains(&RelationshipFacet::Aliases))
        .collect();
    let [edge] = edges.as_slice() else {
        panic!("expected one explicit dependency alias: {edges:?}");
    };
    let target = format!("rift://symbol/cargo/crates.io/{name}/rust/{name}/start");
    assert_eq!(edge.to.as_str(), target);
    let occurrence = edge.occurrence.as_ref().expect("export occurrence");
    let source = &publication
        .units
        .iter()
        .find(|unit| unit.unit == occurrence.unit)
        .expect("original export unit")
        .source;
    let range = usize::try_from(occurrence.range.start).expect("start")
        ..usize::try_from(occurrence.range.end).expect("end");
    assert_eq!(&source[range], "pub use selected::start as launch;");
}
