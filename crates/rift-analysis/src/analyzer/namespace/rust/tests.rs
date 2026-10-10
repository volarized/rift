use std::collections::BTreeMap;

use rift_core::{ContributionOrigin, ProjectPath};
use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner},
    read::{PackageIdentity, SourceKind, SourceLocation},
};
use rift_syntax::{RustSyntaxProvider, SyntaxProvider, SyntaxSource};

use crate::{ExactPackageInput, ExactPackageLimits, NamespaceInput, PackageSource};

const MANIFEST: &str = "[package]\nname='crate-root'\nversion='1.0.0'\n";

fn anchors(
    metadata: &[(&str, &str)],
    path: &str,
    source: &str,
) -> BTreeMap<String, rift_core::SymbolId> {
    anchors_with_qualified_name(metadata, path, source, None)
}

fn anchors_with_qualified_name(
    metadata: &[(&str, &str)],
    path: &str,
    source: &str,
    qualified_name: Option<&str>,
) -> BTreeMap<String, rift_core::SymbolId> {
    selected_anchors(metadata, &[(path, source)], qualified_name, false)
        .remove(path)
        .unwrap_or_default()
}

fn selected_anchors(
    metadata: &[(&str, &str)],
    selected: &[(&str, &str)],
    qualified_name: Option<&str>,
    restored_paths: bool,
) -> super::super::Anchors {
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("exact owner")
    .owner()
    .clone();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "crate-root".to_owned(),
                version: "1.0.0".to_owned(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("actual owner origin");
    selected_anchors_for_owner(
        metadata,
        selected,
        qualified_name,
        restored_paths,
        &owner,
        &origin,
    )
}

fn selected_anchors_for_owner(
    metadata: &[(&str, &str)],
    selected: &[(&str, &str)],
    qualified_name: Option<&str>,
    restored_paths: bool,
    owner: &rift_protocol::identity::SymbolOwner,
    origin: &ContributionOrigin,
) -> super::super::Anchors {
    let language = rift_syntax::ShippedLanguage::Rust.language();
    let paths = selected
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("selected physical path"))
        .collect::<Vec<_>>();
    let files = paths
        .iter()
        .zip(selected)
        .map(|(path, (_, source))| PackageSource::new(path, source))
        .collect::<Vec<_>>();
    let metadata_paths = metadata
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("captured metadata path"))
        .collect::<Vec<_>>();
    let context = metadata_paths
        .iter()
        .zip(metadata)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let limits = ExactPackageLimits::new(20, 16 * 1024 * 1024);
    let input = match owner {
        rift_protocol::identity::SymbolOwner::Local
        | rift_protocol::identity::SymbolOwner::NamedLocal { .. } => {
            NamespaceInput::project(owner, &language, &files, &context, &[], limits)
                .expect("bounded project context")
        }
        _ => ExactPackageInput::new(owner, &language, origin, &files, limits)
            .expect("bounded input")
            .with_framework_context(&context, &[])
            .expect("captured Cargo metadata")
            .namespace_input(),
    };
    let facts = files
        .iter()
        .map(|file| {
            let source = file.text();
            let facts = RustSyntaxProvider::default()
                .analyze(
                    SyntaxSource {
                        path: file.path(),
                        text: source,
                    },
                    input.syntax(),
                )
                .expect("parsed Rust source")
                .into_facts();
            if qualified_name.is_none() && !restored_paths {
                return (*facts).clone();
            }
            let mut symbols = facts.symbols().to_vec();
            if let Some(qualified_name) = qualified_name {
                symbols[0].qualified_name = qualified_name.to_owned();
            }
            if restored_paths {
                for symbol in &mut symbols {
                    symbol.module_path = None;
                }
            }
            rift_syntax::SyntaxFacts::from_parts(
                source,
                input.syntax(),
                rift_syntax::SyntaxFactsParts {
                    origin: facts.origin(),
                    language: facts.language().clone(),
                    symbols,
                    has_errors: facts.has_errors(),
                    left_out_declarations: 0,
                    markdown_facts: None,
                    export_bindings: None,
                    source_digest: rift_core::FileDigest::of(source.as_bytes()),
                },
            )
            .expect("checked captured syntax facts")
        })
        .collect::<Vec<_>>();
    let admitted = files
        .iter()
        .zip(&facts)
        .map(|(file, syntax)| super::super::SelectedFile {
            path: file.path(),
            source: file.text(),
            syntax,
        })
        .collect::<Vec<_>>();
    let units = paths
        .iter()
        .map(|path| physical_unit(owner, path))
        .collect::<Vec<_>>();
    let result = super::prepare(&input, &admitted);
    assert_original_sources(&input, selected, &units);
    result
}

fn assert_original_sources(
    input: &NamespaceInput<'_>,
    selected: &[(&str, &str)],
    units: &[rift_core::SourceUnitId],
) {
    for ((file, (path, source)), unit) in input.files().iter().zip(selected).zip(units) {
        assert_eq!(file.path().as_str(), *path);
        assert_eq!(file.text(), *source);
        assert_eq!(
            physical_unit(
                input.owner(),
                &ProjectPath::new(*path).expect("unchanged physical path")
            ),
            *unit
        );
    }
}

fn physical_unit(
    owner: &rift_protocol::identity::SymbolOwner,
    path: &ProjectPath,
) -> rift_core::SourceUnitId {
    match owner {
        rift_protocol::identity::SymbolOwner::Local
        | rift_protocol::identity::SymbolOwner::NamedLocal { .. } => {
            rift_syntax::source_unit_for_path(path).expect("physical project unit")
        }
        _ => rift_core::SourceUnitId::for_owner(owner.clone(), path.as_str())
            .expect("physical released unit"),
    }
}

#[test]
fn captured_cargo_library_root_and_inline_members_have_logical_namespace() {
    let found = anchors(
        &[("Cargo.toml", MANIFEST)],
        "src/lib.rs",
        "pub fn start() {}\npub mod inner { pub struct Runner; impl Runner { pub fn run(&self) {} } }",
    );
    assert_eq!(
        found["start"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/start"
    );
    assert_eq!(
        found["inner::Runner::run"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/inner/Runner/run"
    );
    assert!(
        !found
            .values()
            .any(|identity| identity.as_str().contains("src/lib.rs"))
    );
}

#[test]
fn unicode_source_names_keep_distinct_identities_and_original_ranges() {
    let source = "pub fn café() {}\npub fn cafè() {}\npub mod marché { pub fn début() {} }\npub fn r#café() {}\npub fn cafe\u{301}() {}";
    let found = anchors(&[("Cargo.toml", MANIFEST)], "src/lib.rs", source);
    assert_eq!(found.len(), 6);
    assert_ne!(found["café"], found["cafè"]);
    assert_eq!(found["café"], found["r#café"]);
    assert_eq!(found["café"], found["cafe\u{301}"]);
    for (name, expected) in [
        ("café", vec!["crate_root", "café"]),
        ("cafè", vec!["crate_root", "cafè"]),
        ("marché", vec!["crate_root", "marché"]),
        ("marché::début", vec!["crate_root", "marché", "début"]),
        ("r#café", vec!["crate_root", "café"]),
        ("cafe\u{301}", vec!["crate_root", "café"]),
    ] {
        let identity =
            SymbolIdentity::parse(found[name].as_str()).expect("canonical Unicode identity");
        assert_eq!(identity.qualified_path(), expected);
    }
    let path = ProjectPath::new("src/lib.rs").expect("original path");
    let facts = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            ExactPackageLimits::new(20, 16 * 1024 * 1024).syntax(),
        )
        .expect("parsed Unicode identifiers")
        .into_facts();
    assert!(!facts.has_errors());
    for (name, declaration) in [
        ("café", "pub fn café() {}"),
        ("cafè", "pub fn cafè() {}"),
        ("marché::début", "pub fn début() {}"),
        ("r#café", "pub fn r#café() {}"),
        ("cafe\u{301}", "pub fn cafe\u{301}() {}"),
    ] {
        let symbol = facts
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == name)
            .expect("original declaration");
        let start = source.find(declaration).expect("original source bytes");
        let range_start = usize::try_from(symbol.item_range.start).expect("bounded source range");
        let range_end = usize::try_from(symbol.item_range.end).expect("bounded source range");
        assert_eq!(range_start, start);
        assert_eq!(range_end, start + declaration.len());
        assert_eq!(source.get(range_start..range_end), Some(declaration));
    }
}

#[test]
fn invalid_source_identifiers_and_unicode_crate_names_stay_unresolved() {
    for source in [
        "pub fn 1café() {}",
        "pub fn café-() {}",
        "pub fn ca\u{200c}fé() {}",
        "pub fn ca\u{200d}fé() {}",
        "pub fn r#self() {}",
        "pub fn r#Self() {}",
        "pub fn r#super() {}",
        "pub fn r#crate() {}",
        "pub fn r#_() {}",
    ] {
        assert!(
            anchors(&[("Cargo.toml", MANIFEST)], "src/lib.rs", source).is_empty(),
            "{source:?}"
        );
    }
    let manifest = format!("{MANIFEST}[lib]\nname='café'\n");
    assert!(
        anchors(
            &[("Cargo.toml", &manifest)],
            "src/lib.rs",
            "pub fn valid() {}"
        )
        .is_empty()
    );
}

#[test]
fn explicit_library_name_and_path_replace_physical_layout() {
    let found = anchors(
        &[(
            "nested/Cargo.toml",
            "[package]\nname='crate-root'\nversion='1.0.0'\nautolib=false\n[lib]\nname='actual_library'\npath='source/entry.rs'\n",
        )],
        "nested/source/entry.rs",
        "pub struct Runner;",
    );
    assert_eq!(
        found["Runner"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/actual_library/Runner"
    );
}

#[test]
fn duplicate_occurrence_retains_full_immutable_source_digest() {
    let source = "pub fn repeat() {}\npub fn repeat() {}";
    let found = anchors(&[("Cargo.toml", MANIFEST)], "src/lib.rs", source);
    let expected = format!(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/repeat~2?rev={}",
        crate::documentation::content_digest(source.as_bytes()).0
    );
    assert_eq!(found["repeat~2"].as_str(), expected);
}

#[test]
fn suffix_without_repeated_declared_name_stays_unresolved() {
    assert!(
        anchors_with_qualified_name(
            &[("Cargo.toml", MANIFEST)],
            "src/lib.rs",
            "pub fn repeat() {}",
            Some("repeat~2"),
        )
        .is_empty()
    );
}

#[test]
fn absent_malformed_wrong_owner_and_disabled_library_metadata_stay_unresolved() {
    for metadata in [
        vec![],
        vec![("Cargo.toml", "not a Cargo manifest")],
        vec![("Cargo.toml", "[package]\nname='other'\nversion='1.0.0'\n")],
        vec![(
            "Cargo.toml",
            "[package]\nname='crate-root'\nversion='2.0.0'\n",
        )],
        vec![(
            "Cargo.toml",
            "[package]\nname='crate-root'\nversion='1.0.0'\nautolib=false\n",
        )],
        vec![(
            "Cargo.toml",
            "[package]\nname='crate-root'\nversion.workspace=true\n",
        )],
    ] {
        assert!(anchors(&metadata, "src/lib.rs", "pub struct Runner;").is_empty());
    }
}

#[test]
fn external_module_file_is_unresolved_without_selected_module_graph() {
    assert!(
        anchors(
            &[("Cargo.toml", MANIFEST)],
            "src/task.rs",
            "pub struct Runner;"
        )
        .is_empty()
    );
}

#[test]
fn competing_library_roots_stay_unresolved() {
    let outer = "[package]\nname='crate-root'\nversion='1.0.0'\n[lib]\npath='nested/src/lib.rs'\n";
    assert!(
        anchors(
            &[("Cargo.toml", outer), ("nested/Cargo.toml", MANIFEST)],
            "nested/src/lib.rs",
            "pub struct Runner;"
        )
        .is_empty()
    );
}

#[test]
fn unsafe_library_paths_and_unsupported_crate_names_stay_unresolved() {
    for field in [
        "path='../lib.rs'",
        "path='/lib.rs'",
        "path=''",
        "name='wrong-name'",
        "name=''",
        "name='4wrong'",
    ] {
        let manifest = format!("{MANIFEST}[lib]\n{field}\n");
        assert!(
            anchors(
                &[("Cargo.toml", &manifest)],
                "src/lib.rs",
                "pub struct Runner;"
            )
            .is_empty()
        );
    }
}

#[test]
fn parser_errors_do_not_establish_library_declarations() {
    assert!(anchors(&[("Cargo.toml", MANIFEST)], "src/lib.rs", "pub fn broken(").is_empty());
}

#[test]
fn helper_uses_exact_package_owner_without_runtime_or_registry_defaults() {
    let found = anchors(
        &[("Cargo.toml", MANIFEST)],
        "src/lib.rs",
        "pub struct Runner;",
    );
    let identity =
        SymbolIdentity::parse(found["Runner"].as_str()).expect("canonical logical identity");
    assert!(
        matches!(identity.owner(), SymbolOwner::Package {manager,registry,name,version} if manager == "cargo" && registry == "crates.io" && name == "crate-root" && version == "1.0.0")
    );
}

#[test]
fn declared_sibling_modules_keep_distinct_logical_paths() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "mod a; mod b;"),
            ("src/a.rs", "pub struct Sender;"),
            ("src/b.rs", "pub struct Sender;"),
        ],
        None,
        false,
    );
    assert_eq!(
        found["src/a.rs"]["Sender"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/a/Sender"
    );
    assert_eq!(
        found["src/b.rs"]["Sender"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/b/Sender"
    );
    assert_ne!(found["src/a.rs"]["Sender"], found["src/b.rs"]["Sender"]);
}

#[test]
fn declared_default_modules_follow_file_and_inline_directories() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "mod a; mod outer { mod leaf; }"),
            ("src/a.rs", "mod b; pub struct First;"),
            ("src/a/b/mod.rs", "mod c; pub struct Second;"),
            ("src/a/b/c.rs", "pub struct Third;"),
            ("src/outer/leaf.rs", "pub fn run() {}"),
        ],
        None,
        false,
    );
    assert_eq!(
        found["src/a/b/c.rs"]["Third"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/a/b/c/Third"
    );
    assert_eq!(
        found["src/outer/leaf.rs"]["run"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/outer/leaf/run"
    );
}

#[test]
fn documented_modules_keep_default_external_and_inline_graph() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            (
                "src/lib.rs",
                "/// External module.\nmod a;\n/// Inline module.\nmod outer {\n    /// Nested module.\n    mod leaf;\n}\n",
            ),
            ("src/a.rs", "pub struct Sender;"),
            ("src/outer/leaf.rs", "pub fn run() {}"),
        ],
        None,
        false,
    );
    assert_eq!(
        found["src/a.rs"]["Sender"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/a/Sender"
    );
    assert_eq!(
        found["src/outer/leaf.rs"]["run"].as_str(),
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/outer/leaf/run"
    );
}

#[test]
fn default_module_paths_use_logical_names_and_keep_external_ascii_rules() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            (
                "src/lib.rs",
                "mod r#type; mod marché { mod leaf; } mod café;",
            ),
            ("src/type.rs", "pub fn first() {}"),
            ("src/marché/leaf.rs", "pub fn next() {}"),
            ("src/café.rs", "pub fn refused() {}"),
        ],
        None,
        false,
    );
    for (path, name, expected) in [
        ("src/type.rs", "first", vec!["crate_root", "type", "first"]),
        (
            "src/marché/leaf.rs",
            "next",
            vec!["crate_root", "marché", "leaf", "next"],
        ),
    ] {
        let identity =
            SymbolIdentity::parse(found[path][name].as_str()).expect("canonical module identity");
        assert_eq!(identity.qualified_path(), expected);
    }
    assert!(!found.contains_key("src/café.rs"));
}

#[test]
fn ambiguous_default_module_files_remain_unresolved() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "mod a; pub struct Root;"),
            ("src/a.rs", "pub struct Sender;"),
            ("src/a/mod.rs", "pub struct Sender;"),
        ],
        None,
        false,
    );
    assert!(found["src/lib.rs"].contains_key("Root"));
    assert!(!found.contains_key("src/a.rs"));
    assert!(!found.contains_key("src/a/mod.rs"));
}

#[test]
fn uncaptured_and_unlinked_module_files_do_not_supply_namespaces() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "mod absent; pub struct Root;"),
            ("src/unlinked.rs", "pub struct Sender;"),
        ],
        None,
        false,
    );
    assert!(found["src/lib.rs"].contains_key("Root"));
    assert!(!found.contains_key("src/unlinked.rs"));
}

#[test]
fn path_attributes_and_unknown_inline_attributes_refuse_default_file_mapping() {
    for root in [
        "#[path = \"a.rs\"] mod a;",
        "#[cfg_attr(feature = \"other\", path = \"a.rs\")] mod a;",
        "#[custom] mod outer { mod a; }",
        "mod outer { #![path = \"custom\"] mod a; }",
        "mod outer { #![cfg_attr(feature = \"other\", path = \"custom\")] mod a; }",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", root),
                ("src/a.rs", "pub struct Sender;"),
                ("src/outer/a.rs", "pub struct Sender;"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/a.rs"), "{root}");
        assert!(!found.contains_key("src/outer/a.rs"), "{root}");
    }
}

#[test]
fn restored_module_facts_without_path_state_remain_unresolved() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "mod a; pub struct Root;"),
            ("src/a.rs", "pub struct Sender;"),
        ],
        None,
        true,
    );
    assert!(found["src/lib.rs"].contains_key("Root"));
    assert!(!found.contains_key("src/a.rs"));
}

#[test]
fn cyclic_default_module_assignment_refuses_competing_namespaces() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[("src/lib.rs", "mod lib; pub struct Root;")],
        None,
        false,
    );
    assert!(found.is_empty());
}

#[test]
fn captured_library_name_above_canonical_identity_bound_stays_unresolved() {
    let metadata = format!(
        "{MANIFEST}\n[lib]\nname = '{}'\n",
        "x".repeat(rift_protocol::identity::SYMBOL_ID_BYTES_MAX)
    );
    let found = selected_anchors(
        &[("Cargo.toml", &metadata)],
        &[
            ("src/lib.rs", "mod a; pub struct Root;"),
            ("src/a.rs", "pub struct Sender;"),
        ],
        None,
        false,
    );
    assert!(found.is_empty());
}

fn runtime_anchors(metadata: &[(&str, &str)], selected: &[(&str, &str)]) -> super::super::Anchors {
    let owner = rift_protocol::identity::SymbolOwner::Runtime {
        runtime: "rust".into(),
        version: "1.98.1".into(),
    };
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Stdlib {
            runtime: Some(rift_protocol::read::RuntimeIdentity {
                runtime: "rust".into(),
                version: "1.98.1".into(),
            }),
        }),
        SourceKind::Authored,
    )
    .expect("verified runtime origin");
    selected_anchors_for_owner(metadata, selected, None, false, &owner, &origin)
}

fn project_anchors(
    owner: &rift_protocol::identity::SymbolOwner,
    metadata: &[(&str, &str)],
    selected: &[(&str, &str)],
) -> super::super::Anchors {
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Project { package: None }),
        SourceKind::Authored,
    )
    .expect("selected project origin");
    selected_anchors_for_owner(metadata, selected, None, false, owner, &origin)
}

#[test]
fn local_cargo_root_and_modules_keep_local_owner() {
    let found = project_anchors(
        &rift_protocol::identity::SymbolOwner::Local,
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "mod a; pub struct Root;"),
            ("src/a.rs", "pub struct Sender;"),
        ],
    );
    assert_eq!(
        found["src/lib.rs"]["Root"].as_str(),
        "rift://symbol/local/rust/crate_root/Root"
    );
    assert_eq!(
        found["src/a.rs"]["Sender"].as_str(),
        "rift://symbol/local/rust/crate_root/a/Sender"
    );
    assert_eq!(found.len(), 2);
}

#[test]
fn named_project_keeps_registered_owner_and_captured_crate_target() {
    let owner = rift_protocol::identity::SymbolOwner::NamedLocal {
        name: "extra".into(),
    };
    let found = project_anchors(
        &owner,
        &[(
            "Cargo.toml",
            "[package]\nname='beacon'\nversion.workspace=true\n[lib]\nname='compass'\npath='root.rs'\n",
        )],
        &[("root.rs", "pub struct Root;")],
    );
    assert_eq!(
        found["root.rs"]["Root"].as_str(),
        "rift://symbol/local@extra/rust/compass/Root"
    );
    assert_eq!(found.len(), 1);
}

#[test]
fn local_workspace_without_captured_library_target_stays_unresolved() {
    assert!(
        project_anchors(
            &rift_protocol::identity::SymbolOwner::Local,
            &[("Cargo.toml", "[workspace]\nmembers=['beacon']\n")],
            &[("src/lib.rs", "pub struct Root;")],
        )
        .is_empty()
    );
}

#[test]
fn named_project_refuses_competing_captured_crate_roots() {
    let owner = rift_protocol::identity::SymbolOwner::NamedLocal {
        name: "extra".into(),
    };
    assert!(
        project_anchors(
            &owner,
            &[("a/Cargo.toml", MANIFEST), ("b/Cargo.toml", MANIFEST)],
            &[
                ("a/src/lib.rs", "pub struct Root;"),
                ("b/src/lib.rs", "pub struct Root;")
            ],
        )
        .is_empty()
    );
}

#[test]
fn runtime_cargo_roots_keep_acquisition_version_and_distinct_crates() {
    let found = runtime_anchors(
        &[
            (
                "core/Cargo.toml",
                "[package]\nname='core'\nversion='0.0.0'\n[lib]\ntest=false\n",
            ),
            (
                "alloc/Cargo.toml",
                "[package]\nname='alloc'\nversion='0.0.0'\n[lib]\ntest=false\n",
            ),
            (
                "std/Cargo.toml",
                "[package]\nname='std'\nversion='0.0.0'\n[lib]\ncrate-type=['dylib','rlib']\n",
            ),
        ],
        &[
            ("core/src/lib.rs", "mod option; pub struct Root;"),
            ("core/src/option.rs", "pub enum Option { None }"),
            ("alloc/src/lib.rs", "pub struct Root;"),
            ("std/src/lib.rs", "pub mod io { pub fn read() {} }"),
        ],
    );
    assert_eq!(
        found["core/src/lib.rs"]["Root"].as_str(),
        "rift://symbol/stdlib/rust@1.98.1/rust/core/Root"
    );
    assert_eq!(
        found["core/src/option.rs"]["Option"].as_str(),
        "rift://symbol/stdlib/rust@1.98.1/rust/core/option/Option"
    );
    assert_eq!(
        found["alloc/src/lib.rs"]["Root"].as_str(),
        "rift://symbol/stdlib/rust@1.98.1/rust/alloc/Root"
    );
    assert_eq!(
        found["std/src/lib.rs"]["io::read"].as_str(),
        "rift://symbol/stdlib/rust@1.98.1/rust/std/io/read"
    );
    assert_eq!(found.len(), 4);
}

#[test]
fn runtime_workspace_and_tools_require_selected_library_targets() {
    let found = runtime_anchors(
        &[
            ("Cargo.toml", "[workspace]\nmembers=['core','tool']"),
            ("core/Cargo.toml", "[package]\nname='core'\nversion='0.0.0'"),
            ("tool/Cargo.toml", "[package]\nname='tool'\nversion='0.1.0'"),
            (
                "disabled/Cargo.toml",
                "[package]\nname='disabled'\nversion='0.1.0'\nautolib=false",
            ),
        ],
        &[
            ("core/src/lib.rs", "pub struct Root;"),
            ("tool/src/main.rs", "fn main() {}"),
            ("disabled/src/lib.rs", "pub struct Unproved;"),
        ],
    );
    assert_eq!(found.len(), 1);
    assert!(found.contains_key("core/src/lib.rs"));
}

#[test]
fn runtime_duplicate_names_and_unsupported_paths_remain_unresolved() {
    let found = runtime_anchors(
        &[
            ("core/Cargo.toml", "[package]\nname='core'\nversion='0.0.0'"),
            (
                "first/Cargo.toml",
                "[package]\nname='compiler_builtins'\nversion='0.1.160'",
            ),
            (
                "second/Cargo.toml",
                "[package]\nname='compiler_builtins'\nversion='0.1.160'\n[lib]\npath='../first/src/lib.rs'",
            ),
            (
                "escape/Cargo.toml",
                "[package]\nname='escape'\nversion='0.0.0'\n[lib]\npath='../core/src/lib.rs'",
            ),
        ],
        &[
            ("core/src/lib.rs", "pub struct Root;"),
            ("first/src/lib.rs", "pub fn intrinsic() {}"),
            ("second/src/lib.rs", "pub fn intrinsic() {}"),
            ("escape/src/lib.rs", "pub struct Unproved;"),
        ],
    );
    assert_eq!(found.len(), 1);
    assert!(found.contains_key("core/src/lib.rs"));
}

#[test]
fn runtime_explicit_target_and_inherited_package_version_keep_runtime_owner() {
    let found = runtime_anchors(
        &[(
            "wrapper/Cargo.toml",
            "[package]\nname='rustc-std-workspace-core'\nversion={workspace=true}\n[lib]\npath='lib.rs'\nname='actual_core'",
        )],
        &[("wrapper/lib.rs", "pub struct Root;")],
    );
    assert_eq!(
        found["wrapper/lib.rs"]["Root"].as_str(),
        "rift://symbol/stdlib/rust@1.98.1/rust/actual_core/Root"
    );
}
