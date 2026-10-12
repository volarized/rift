use std::sync::Arc;

use rift_core::{FileDigest, ProjectPath};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::Language;
use rift_syntax::{SyntaxLimits, SyntaxSource, registry};

use crate::{ExactPackageLimits, IndexedFile, NamespaceInput, PackageSource, WorkspaceSemantics};

fn source() -> IndexedFile {
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let text = "pub fn start() {}";
    let syntax = registry::provider_for_extension("rs")
        .expect("Rust provider")
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("Rust syntax");
    IndexedFile::new(
        path,
        Arc::new(text.to_owned()),
        FileDigest::of(text.as_bytes()),
        false,
        syntax,
    )
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
