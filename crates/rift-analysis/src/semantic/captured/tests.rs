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
    WorkspaceSemantics::build_captured_project_facts(
        input,
        files,
        64,
        64,
        revision,
        previous.map(WorkspaceSemantics::graph),
    )
    .expect("captured static semantics")
    .semantics
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
        assert_eq!(call.target_binding.unit().to_string(), expected);
        assert_eq!(
            call.source_digest,
            FileDigest::of(source.source().as_bytes())
        );
    }
    let deleted_view = static_project(&[&edited], 9, Some(&edited_view));
    assert!(deleted_view.static_occurrences().is_empty());
    assert_eq!(original_view.static_occurrences().len(), 2);
    assert_eq!(edited_view.static_occurrences().len(), 2);
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
        let built = WorkspaceSemantics::build_captured_project_facts(
            input,
            &[&file],
            16,
            16,
            revision,
            captured.as_ref().map(WorkspaceSemantics::graph),
        )
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
