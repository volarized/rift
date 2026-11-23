use std::sync::Arc;

use rift_core::{FileDigest, ProjectPath};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::Language;
use rift_syntax::{SyntaxLimits, SyntaxSource, registry};

use crate::{ExactPackageLimits, IndexedFile, NamespaceInput, PackageSource, WorkspaceSemantics};

fn source() -> IndexedFile {
    source_file("src/lib.rs", "pub fn start() {}")
}

fn source_file(path: &str, text: &str) -> IndexedFile {
    let path = ProjectPath::new(path).expect("source path");
    let extension = path.as_str().rsplit_once('.').expect("source extension").1;
    let syntax = registry::provider_for_extension(extension)
        .expect("syntax provider")
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("source syntax");
    IndexedFile::new(
        path,
        Arc::new(text.to_owned()),
        FileDigest::of(text.as_bytes()),
        false,
        syntax,
    )
}

fn static_project(
    files: &[&IndexedFile],
    revision: u64,
    previous: Option<&WorkspaceSemantics>,
) -> WorkspaceSemantics {
    let language = Language::from_identity_segment("javascript").expect("JavaScript language");
    let sources = files
        .iter()
        .map(|file| PackageSource::new(file.path(), file.source()))
        .collect::<Vec<_>>();
    let input = NamespaceInput::project(
        &SymbolOwner::Local,
        &language,
        &sources,
        &[],
        &[],
        ExactPackageLimits::new(16, 32_768),
    )
    .expect("captured context");
    WorkspaceSemantics::prepare_captured_project_facts_after(input, files, 64, revision, previous)
        .expect("captured admission")
        .build(64, previous.map(WorkspaceSemantics::graph))
        .expect("captured static semantics")
        .semantics
}

#[test]
fn unchanged_admitted_files_share_placements_and_facts_across_held_revisions() {
    use get_size2::{GetSize, StandardTracker};
    let stable = source_file("stable.mjs", "export function stable() {}");
    let old = source_file("changed.mjs", "export function initial() {}");
    let changed = source_file("changed.mjs", "/* moved */ export function fresh() {}");
    let first = static_project(&[&stable, &old], 1, None);
    let second = static_project(&[&stable, &changed], 2, Some(&first));
    let third = static_project(&[&stable, &old], 3, Some(&second));
    let cold = static_project(&[&stable, &old], 3, None);
    let first_stable = &first.captured_sources.0[stable.path()];
    for view in [&second, &third] {
        let current = &view.captured_sources.0[stable.path()];
        assert!(Arc::ptr_eq(&first_stable.placement, &current.placement));
        assert!(first_stable.facts.ptr_eq(&current.facts));
        let row = |view: &WorkspaceSemantics| {
            let key = &view.graph().records()[0].contributions()[0];
            view.graph()
                .contribution(key)
                .expect("provider row")
                .facts()
                .expect("facts")
                .name()
                .as_ptr()
        };
        assert_eq!(row(&first), row(view));
    }
    assert!(!Arc::ptr_eq(
        &first.captured_sources.0[old.path()].placement,
        &second.captured_sources.0[changed.path()].placement,
    ));
    assert_eq!(third.graph().records(), cold.graph().records());
    assert_eq!(third.graph().references(), cold.graph().references());
    assert_eq!(third.graph().edges(), cold.graph().edges());
    let (first_bytes, tracker) = first.get_heap_size_with_tracker(StandardTracker::new());
    let (second_bytes, tracker) = second.get_heap_size_with_tracker(tracker);
    let (third_bytes, _) = third.get_heap_size_with_tracker(tracker);
    eprintln!(
        "held semantic material bytes by generation: {first_bytes}, {second_bytes}, {third_bytes}"
    );
    assert!(first_bytes > 0 && second_bytes > 0 && third_bytes > 0);
    assert!(third_bytes < cold.get_heap_size_with_tracker(StandardTracker::new()).0);
    let bounded_files = [&stable, &old];
    let sources = bounded_files.map(|f| PackageSource::new(f.path(), f.source()));
    let language = Language::from_identity_segment("javascript").expect("language");
    let namespace = NamespaceInput::project(
        &SymbolOwner::Local,
        &language,
        &sources,
        &[],
        &[],
        ExactPackageLimits::new(16, 32_768),
    )
    .expect("namespace");
    let bounded = WorkspaceSemantics::prepare_captured_project_facts_after(
        namespace,
        &bounded_files,
        first_stable.rows.len(),
        4,
        Some(&third),
    )
    .expect("bounded admission");
    assert_eq!(
        bounded.beyond_declaration_bound(),
        std::slice::from_ref(old.path())
    );
    assert!(!bounded.admitted_all());
    drop(bounded);
    let weak = Arc::downgrade(stable.syntax_facts());
    drop(stable);
    assert!(weak.upgrade().is_none());
}

#[test]
fn unsupported_rust_use_keeps_independent_static_calls() {
    let rust = source_file(
        "src/lib.rs",
        "pub use ::dependency::Thing as Foreign; fn leaf() {} pub fn run() { leaf(); }",
    );
    let typescript = source_file(
        "main.ts",
        "function leaf() {} export function run() { leaf(); }",
    );
    let files = [&rust, &typescript];
    let sources = files.map(|file| PackageSource::new(file.path(), file.source()));
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let metadata = [PackageSource::new(
        &manifest,
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
    )];
    let language = Language::from_identity_segment("typescript").expect("TypeScript language");
    let input = NamespaceInput::project(
        &SymbolOwner::Local,
        &language,
        &sources,
        &metadata,
        &[],
        ExactPackageLimits::new(16, 32_768),
    )
    .expect("captured context");
    let view = WorkspaceSemantics::build_captured_project_facts(input, &files, 64, 256, 7, None)
        .expect("captured static semantics")
        .semantics;

    let calls = view
        .static_occurrences()
        .iter()
        .filter(|occurrence| occurrence.role == rift_core::ReferenceRole::Call)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for file in files {
        let call = calls
            .iter()
            .find(|call| call.binding.unit().key().as_str() == file.path().as_str())
            .expect("the file contributes its static call");
        let target = call.target_binding.as_ref().expect("local callee");
        assert_eq!(call.binding.unit(), target.unit());
        assert!(view.static_edge_calls_complete(
            call.binding.unit(),
            Some(rift_syntax::ByteRange {
                start: call.binding.range().start(),
                end: call.binding.range().end(),
            }),
        ));
    }
    assert_eq!(
        view.static_targets_omitted()
            .get(&rift_protocol::read::EdgeKind::Imports),
        Some(&1)
    );
}

#[test]
fn external_targets_keep_their_physical_usages_and_omissions_by_unit() {
    let text = "import { readFileSync } from 'node:fs'; import { lost } from 'undeclared'; export function load() { readFileSync('a'); }";
    let load = source_file("load.mjs", text);
    let other = source_file("other.mjs", "export function other() {}");
    let view = static_project(&[&load, &other], 7, None);
    let target = rift_core::SymbolId::new("rift://symbol/stdlib/node/javascript/fs/readFileSync")
        .expect("runtime identity");
    let usages = view
        .static_usages(&target)
        .map(|(binding, range, digest, role)| {
            let start = usize::try_from(range.start).expect("start");
            let end = usize::try_from(range.end).expect("end");
            assert_eq!(binding.unit().key().as_str(), "load.mjs");
            assert_eq!(digest, load.digest());
            (&text[start..end], role)
        })
        .collect::<Vec<_>>();
    for role in [
        rift_core::ReferenceRole::Import,
        rift_core::ReferenceRole::Call,
        rift_core::ReferenceRole::Read,
    ] {
        assert!(usages.contains(&("readFileSync", role)), "{usages:?}");
    }
    assert_eq!(usages.len(), 3, "{usages:?}");
    let absent = rift_core::SymbolId::new("rift://symbol/stdlib/node/javascript/fs")
        .expect("runtime identity");
    assert_eq!(view.static_usages(&absent).count(), 0);

    let omitted = |path: &str| {
        let unit = rift_core::SourceUnitId::new(
            rift_core::SourceResolverId::new("project").expect("project resolver"),
            rift_core::SourcePath::new(path).expect("source path"),
        )
        .expect("project unit");
        view.static_targets_omitted_in(&unit).cloned()
    };
    assert_eq!(
        omitted("load.mjs"),
        Some(
            [(rift_protocol::read::EdgeKind::Imports, 1)]
                .into_iter()
                .collect()
        )
    );
    assert_eq!(
        omitted("other.mjs"),
        Some(std::collections::BTreeMap::new())
    );
}

#[test]
fn static_call_targets_follow_edits_and_held_views_keep_original_occurrences() {
    let original = source_file(
        "main.mjs",
        "import { start as run } from './jobs.mjs'; export function caller() { run(); }",
    );
    let old_target = source_file("jobs.mjs", "export function start() {}");
    let original_view = static_project(&[&original, &old_target], 7, None);
    let moved_target = source_file("tasks.mjs", "export function start() {}");
    let edited = source_file(
        "main.mjs",
        "import { start as run } from './tasks.mjs'; export function caller() { run(); }",
    );
    let edited_view = static_project(&[&edited, &moved_target], 8, Some(&original_view));
    let cold_view = static_project(&[&edited, &moved_target], 8, None);
    for (view, source, expected) in [
        (&original_view, &original, "rift://source/project/jobs.mjs"),
        (&edited_view, &edited, "rift://source/project/tasks.mjs"),
        (&cold_view, &edited, "rift://source/project/tasks.mjs"),
    ] {
        let call = view
            .static_occurrences()
            .iter()
            .find(|occurrence| occurrence.role == rift_core::ReferenceRole::Call)
            .expect("captured call");
        assert_eq!(
            call.target_binding
                .as_ref()
                .expect("target declaration")
                .unit()
                .to_string(),
            expected
        );
        assert_eq!(
            call.source_digest,
            FileDigest::of(source.source().as_bytes())
        );
    }
    let deleted_view = static_project(&[&edited], 9, Some(&edited_view));
    assert!(
        !deleted_view
            .static_occurrences()
            .iter()
            .any(|occurrence| occurrence.role == rift_core::ReferenceRole::Call)
    );
    for view in [&original_view, &edited_view] {
        assert_eq!(
            view.static_occurrences()
                .iter()
                .filter(|occurrence| occurrence.role == rift_core::ReferenceRole::Call)
                .count(),
            1
        );
        assert_eq!(
            view.static_occurrences()
                .iter()
                .filter(|occurrence| occurrence.role == rift_core::ReferenceRole::Import)
                .count(),
            1
        );
    }
}

#[test]
fn changed_captured_metadata_changes_identity_without_changing_source_or_history() {
    let file = source();
    let sources = [PackageSource::new(file.path(), file.source())];
    let metadata_path = ProjectPath::new("Cargo.toml").expect("metadata path");
    let language = Language::from_identity_segment("rust").expect("Rust language");
    let owner = SymbolOwner::Local;
    let mut captured = None;
    let mut old_identity = None;
    for (revision, name) in [(7, "beacon"), (8, "compass")] {
        let metadata = format!("[package]\nname='{name}'\nversion='1.0.0'\n");
        let context = [PackageSource::new(&metadata_path, &metadata)];
        let input = NamespaceInput::project(
            &owner,
            &language,
            &sources,
            &context,
            &[],
            ExactPackageLimits::new(2, 256),
        )
        .expect("captured context");
        let prepared = WorkspaceSemantics::prepare_captured_project_facts_after(
            input,
            &[&file],
            16,
            revision,
            captured.as_ref(),
        )
        .expect("admission with current metadata");
        assert!(prepared.admitted[0].1.is_none());
        let built = prepared
            .build(16, captured.as_ref().map(WorkspaceSemantics::graph))
            .expect("captured semantics");
        let assembled = built
            .semantics
            .assembled("rift://symbol/rust/src/lib.rs/start")
            .expect("retained declaration");
        let expected = SymbolIdentity::new(
            owner.clone(),
            language.clone(),
            vec![name.to_owned(), "start".to_owned()],
        )
        .expect("logical namespace")
        .wire_identity();
        assert_eq!(
            assembled.identity().expect("current proof").as_str(),
            expected
        );
        assert_eq!(assembled.facts().expect("retained facts").name(), "start");
        if let Some(previous) = &captured {
            let held = previous
                .assembled("rift://symbol/rust/src/lib.rs/start")
                .expect("historical declaration");
            assert_eq!(
                held.identity().expect("historical proof").as_str(),
                old_identity.as_deref().expect("old identity")
            );
        }
        old_identity = Some(expected);
        captured = Some(built.semantics);
    }
    assert_eq!(file.source(), "pub fn start() {}");
}

#[test]
fn named_owner_preserves_project_unit_and_missing_metadata_does_not_reuse_history() {
    let file = source();
    let sources = [PackageSource::new(file.path(), file.source())];
    let metadata_path = ProjectPath::new("Cargo.toml").expect("metadata path");
    let context = [PackageSource::new(
        &metadata_path,
        "[package]\nname='beacon'\nversion='1.0.0'\n",
    )];
    let language = Language::from_identity_segment("rust").expect("Rust language");
    for owner in [
        SymbolOwner::Local,
        SymbolOwner::NamedLocal {
            name: "cloud".to_owned(),
        },
    ] {
        let input = NamespaceInput::project(
            &owner,
            &language,
            &sources,
            &context,
            &[],
            ExactPackageLimits::new(2, 256),
        )
        .expect("captured context");
        let built =
            WorkspaceSemantics::build_captured_project_facts(input, &[&file], 16, 16, 7, None)
                .expect("current namespace");
        let assembled = built
            .semantics
            .assembled("rift://symbol/rust/src/lib.rs/start")
            .expect("declaration");
        let logical =
            SymbolIdentity::parse(assembled.identity().expect("canonical identity").as_str())
                .expect("shared codec");
        assert_eq!(logical.owner(), &owner);
        for key in assembled.contributions() {
            let contribution = built
                .semantics
                .graph()
                .contribution(key)
                .expect("captured contribution");
            assert_eq!(
                contribution
                    .source()
                    .expect("physical source")
                    .unit()
                    .to_string(),
                "rift://source/project/src/lib.rs"
            );
        }
        let input = NamespaceInput::project(
            &owner,
            &language,
            &sources,
            &[],
            &[],
            ExactPackageLimits::new(1, 256),
        )
        .expect("source without metadata");
        let unknown = WorkspaceSemantics::build_captured_project_facts(
            input,
            &[&file],
            16,
            16,
            8,
            Some(built.semantics.graph()),
        )
        .expect("unresolved current declaration");
        let unknown = unknown
            .semantics
            .assembled("rift://symbol/rust/src/lib.rs/start")
            .expect("retained facts");
        assert!(unknown.identity().is_none());
        assert_eq!(unknown.facts().expect("physical facts").name(), "start");
        assert!(
            built
                .semantics
                .assembled("rift://symbol/rust/src/lib.rs/start")
                .expect("immutable previous declaration")
                .identity()
                .is_some()
        );
    }
}

#[test]
fn captured_source_membership_mismatch_and_duplicate_documents_refuse() {
    let file = source();
    let owner = SymbolOwner::Local;
    let language = Language::from_identity_segment("rust").expect("Rust language");
    let changed = [PackageSource::new(file.path(), "pub fn changed() {}")];
    let input = NamespaceInput::project(
        &owner,
        &language,
        &changed,
        &[],
        &[],
        ExactPackageLimits::new(1, 256),
    )
    .expect("different captured bytes");
    assert!(
        WorkspaceSemantics::build_captured_project_facts(input, &[&file], 16, 16, 7, None).is_err()
    );
    let sources = [PackageSource::new(file.path(), file.source())];
    let input = NamespaceInput::project(
        &owner,
        &language,
        &sources,
        &[],
        &[],
        ExactPackageLimits::new(1, 256),
    )
    .expect("captured source");
    assert!(
        WorkspaceSemantics::build_captured_project_facts(input, &[&file, &file], 16, 16, 7, None)
            .is_err()
    );
}

#[test]
fn typescript_reexports_retain_alias_declarations_and_edges() {
    let barrel = source_file(
        "index.ts",
        "export * from './jobs.js'; export { default as run } from './job.js';",
    );
    let jobs = source_file("jobs.ts", "export function leaf() {}");
    let job = source_file("job.ts", "export default function () { return 1; }");
    let view = static_project(&[&barrel, &jobs, &job], 1, None);
    let aliases = view
        .edges()
        .edges()
        .filter(|edge| edge.kind() == rift_protocol::read::EdgeKind::Aliases)
        .collect::<Vec<_>>();
    assert_eq!(aliases.len(), 2);
    assert!(
        aliases
            .iter()
            .any(|edge| edge.from().as_str().ends_with("index.ts/leaf")
                && edge.to().as_str().ends_with("jobs.ts/leaf"))
    );
    assert!(
        aliases
            .iter()
            .any(|edge| edge.from().as_str().ends_with("index.ts/run")
                && edge.to().as_str().ends_with("job.ts/default"))
    );
    for alias in aliases {
        assert!(
            view.graph()
                .records()
                .iter()
                .any(|record| record.identity() == Some(alias.from()))
        );
    }
}
