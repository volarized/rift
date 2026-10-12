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
    selected_anchors_for_context(
        metadata,
        selected,
        qualified_name,
        restored_paths,
        owner,
        origin,
        false,
    )
}

fn selected_anchors_for_context(
    metadata: &[(&str, &str)],
    selected: &[(&str, &str)],
    qualified_name: Option<&str>,
    restored_paths: bool,
    owner: &rift_protocol::identity::SymbolOwner,
    origin: &ContributionOrigin,
    library: bool,
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
    let limits = ExactPackageLimits::new(21, 16 * 1024 * 1024);
    let input = match owner {
        SymbolOwner::Local | SymbolOwner::NamedLocal { .. } => {
            NamespaceInput::project(owner, &language, &files, &context, &[], limits)
                .expect("bounded project context")
        }
        _ => {
            let input = ExactPackageInput::new(owner, &language, origin, &files, limits)
                .expect("bounded input")
                .with_framework_context(&context, &[])
                .expect("captured Cargo metadata");
            let input = if library {
                selected_library(&input, owner, &context, &files)
            } else {
                input
            };
            input.namespace_input()
        }
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
                    left_out_symbols: facts.left_out_symbols().to_vec(),
                    has_errors: facts.has_errors(),
                    left_out_declarations: 0,
                    markdown_facts: None,
                    export_bindings: None,
                    macro_invocations: facts.macro_invocations().map(<[_]>::to_vec),
                    scope: facts.scope().map(<[_]>::to_vec),
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
    let result = super::prepare(&input, &admitted).anchors;
    assert_original_sources(&input, selected, &units);
    result
}

fn selected_library<'input>(
    input: &ExactPackageInput<'input>,
    owner: &SymbolOwner,
    context: &[PackageSource<'input>],
    files: &[PackageSource<'input>],
) -> ExactPackageInput<'input> {
    let manifest = *context
        .iter()
        .find(|file| file.path().as_str() == "Cargo.toml")
        .expect("original selected manifest");
    let root = *files
        .iter()
        .find(|file| file.path().as_str() == "src/lib.rs")
        .expect("original selected library root");
    (*input)
        .with_library(owner, manifest, root)
        .expect("explicit selected library acquisition")
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
    let supported = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "#[path = \"a.rs\"] mod a;"),
            ("src/a.rs", "pub struct Sender;"),
            ("src/outer/a.rs", "pub struct Sender;"),
        ],
        None,
        false,
    );
    let identity = SymbolIdentity::parse(supported["src/a.rs"]["Sender"].as_str())
        .expect("original explicitly selected module");
    assert_eq!(identity.qualified_path(), vec!["crate_root", "a", "Sender"]);
    assert!(!supported.contains_key("src/outer/a.rs"));
    for root in [
        "#[path = \"missing.rs\"] mod a;",
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

#[test]
fn transparent_item_macros_use_captured_ordered_imports_for_module_roots() {
    let definition = "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] #[cfg_attr(docsrs, doc(cfg(feature = \"rt\")))] $item )* } }";
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            (
                "src/lib.rs",
                "#[macro_use]\n#[doc(hidden)]\nmod macros;\ncfg_rt! { pub mod runtime; }\npub fn neighbor() {}",
            ),
            ("src/macros/mod.rs", "#[macro_use]\nmod cfg;"),
            ("src/macros/cfg.rs", definition),
            ("src/runtime/mod.rs", "mod builder; pub fn start() {}"),
            ("src/runtime/builder.rs", "pub struct Builder;"),
        ],
        None,
        false,
    );
    for (path, name, expected) in [
        (
            "src/runtime/mod.rs",
            "start",
            vec!["crate_root", "runtime", "start"],
        ),
        (
            "src/runtime/builder.rs",
            "Builder",
            vec!["crate_root", "runtime", "builder", "Builder"],
        ),
    ] {
        let identity =
            SymbolIdentity::parse(found[path][name].as_str()).expect("captured module identity");
        assert_eq!(identity.qualified_path(), expected);
    }
    assert!(found["src/lib.rs"].contains_key("neighbor"));
}

#[test]
fn transparent_item_macros_refuse_shadowing_unknown_order_and_changed_items() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    for root in [
        "cfg_rt! { pub mod runtime; }\n#[macro_use] mod macros;",
        "mod macros; cfg_rt! { pub mod runtime; }",
        "#[macro_use] mod macros; macro_rules! cfg_rt { () => {}; } cfg_rt! { pub mod runtime; }",
        "#[macro_use] mod macros; cfg_rt! { #[path = \"renamed.rs\"] pub mod runtime; pub fn mixed() {} }",
        "#[macro_use] mod macros; cfg_rt! { other! { pub mod runtime; } }",
        "#[macro_use] mod macros; macros::cfg_rt! { pub mod runtime; }",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", root),
                ("src/macros.rs", definition),
                ("src/runtime.rs", "pub fn refused() {}"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/runtime.rs"), "{root}");
    }
    for changed in [
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* }; () => {}; }",
        "macro_rules! cfg_rt { ($($item:item)*) => { pub mod runtime; } }",
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[path = \"runtime.rs\"] $item )* } }",
        "#[cfg(feature = \"rt\")] macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                (
                    "src/lib.rs",
                    "#[macro_use] mod macros; cfg_rt! { pub mod runtime; }",
                ),
                ("src/macros.rs", changed),
                ("src/runtime.rs", "pub fn refused() {}"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/runtime.rs"), "{changed}");
    }
}

#[test]
fn transparent_item_macros_keep_direct_modules_in_mixed_item_bodies() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            (
                "src/lib.rs",
                "#[macro_use] mod macros; cfg_rt! { pub mod runtime; } cfg_rt! { pub mod task; }",
            ),
            ("src/macros.rs", definition),
            (
                "src/runtime/mod.rs",
                "cfg_rt! { pub use builder::Builder; mod builder; }",
            ),
            ("src/runtime/builder.rs", "pub struct Builder;"),
            (
                "src/task/mod.rs",
                "cfg_rt! { pub use crate::runtime::{Builder}; mod spawn; pub use spawn::spawn; unknown! { mod hidden; } }",
            ),
            ("src/task/spawn.rs", "pub fn spawn() {}"),
            ("src/task/hidden.rs", "pub fn refused() {}"),
        ],
        None,
        false,
    );
    for (path, name, expected) in [
        (
            "src/runtime/builder.rs",
            "Builder",
            vec!["crate_root", "runtime", "builder", "Builder"],
        ),
        (
            "src/task/spawn.rs",
            "spawn",
            vec!["crate_root", "task", "spawn", "spawn"],
        ),
    ] {
        let identity = SymbolIdentity::parse(found[path][name].as_str())
            .expect("captured direct module identity");
        assert_eq!(identity.qualified_path(), expected);
    }
    assert!(!found.contains_key("src/task/hidden.rs"));
}

#[test]
fn transparent_item_macros_accept_forwarding_to_earlier_definitions() {
    let definitions = "macro_rules! cfg_process { ($($item:item)*) => { $( #[cfg(feature = \"process\")] $item )* } }\nmacro_rules! cfg_process_driver { ($($item:item)*) => { #[cfg(unix)] #[cfg(not(loom))] cfg_process! { $($item)* } } }";
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            (
                "src/lib.rs",
                "#[macro_use] mod macros; cfg_process_driver! { pub mod runtime; }",
            ),
            ("src/macros.rs", definitions),
            ("src/runtime.rs", "pub struct Runtime;"),
        ],
        None,
        false,
    );
    let identity = SymbolIdentity::parse(found["src/runtime.rs"]["Runtime"].as_str())
        .expect("captured forwarding module identity");
    assert_eq!(
        identity.qualified_path(),
        ["crate_root", "runtime", "Runtime"]
    );
}

#[test]
fn transparent_item_macros_refuse_unproved_forwarding_and_changed_items() {
    let target = "macro_rules! cfg_process { ($($item:item)*) => { $( #[cfg(feature = \"process\")] $item )* } }";
    for definitions in [
        "macro_rules! forward { ($($item:item)*) => { #[cfg(unix)] unknown! { $($item)* } } }"
            .to_owned(),
        format!(
            "macro_rules! forward {{ ($($item:item)*) => {{ #[cfg(unix)] cfg_process! {{ $($item)* }} }} }} {target}"
        ),
        "macro_rules! forward { ($($item:item)*) => { #[cfg(unix)] forward! { $($item)* } } }"
            .to_owned(),
        format!(
            "{target} macro_rules! cfg_process {{ () => {{}}; }} macro_rules! forward {{ ($($item:item)*) => {{ #[cfg(unix)] cfg_process! {{ $($item)* }} }} }}"
        ),
        format!(
            "{target} macro_rules! forward {{ ($($item:item)*) => {{ #[cfg(unix)] cfg_process! {{ pub mod runtime; }} }} }}"
        ),
        format!(
            "{target} macro_rules! forward {{ ($($item:item)*) => {{ #[path = \"runtime.rs\"] cfg_process! {{ $($item)* }} }} }}"
        ),
        format!(
            "{target} macro_rules! forward {{ ($($item:item)*) => {{ #[cfg(unix)] cfg_process! {{ $($item)* }} }}; () => {{}}; }}"
        ),
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                (
                    "src/lib.rs",
                    "#[macro_use] mod macros; forward! { pub mod runtime; }",
                ),
                ("src/macros.rs", &definitions),
                ("src/runtime.rs", "pub struct Runtime;"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/runtime.rs"), "{definitions}");
    }
}

#[test]
fn selected_non_test_library_context_requires_original_owner_manifest_and_root() {
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
    let language = rift_syntax::ShippedLanguage::Rust.language();
    let manifest_path = ProjectPath::new("Cargo.toml").expect("manifest path");
    let root_path = ProjectPath::new("src/lib.rs").expect("library path");
    let missing_path = ProjectPath::new("src/missing.rs").expect("missing source path");
    let files = [PackageSource::new(&root_path, "pub struct Example;\n")];
    let metadata = [PackageSource::new(&manifest_path, MANIFEST)];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(20, 16 * 1024 * 1024),
    )
    .expect("bounded original sources")
    .with_framework_context(&metadata, &[])
    .expect("original metadata");
    assert!(input.namespace_input().library().is_none());
    let accepted = input
        .with_library(&owner, metadata[0], files[0])
        .expect("explicit selected library role");
    let (manifest, root) = accepted
        .namespace_input()
        .library()
        .expect("captured selected context");
    assert_eq!(manifest.path(), &manifest_path);
    assert_eq!(manifest.text(), MANIFEST);
    assert_eq!(root.path(), &root_path);
    assert_eq!(root.text(), files[0].text());
    assert!(
        input
            .with_library(
                &rift_protocol::identity::SymbolOwner::Local,
                metadata[0],
                files[0]
            )
            .is_err()
    );
    assert!(
        input
            .with_library(
                &owner,
                PackageSource::new(
                    &manifest_path,
                    "[package]\nname='foreign'\nversion='1.0.0'\n"
                ),
                files[0]
            )
            .is_err()
    );
    assert!(
        input
            .with_library(
                &owner,
                metadata[0],
                PackageSource::new(&missing_path, files[0].text())
            )
            .is_err()
    );
    assert!(
        input
            .with_library(
                &owner,
                metadata[0],
                PackageSource::new(&root_path, "pub struct Changed;\n")
            )
            .is_err()
    );
    assert!(accepted.with_framework_context(&[], &[]).is_err());
}

#[test]
fn selected_library_context_refuses_admitted_manifest_with_wrong_release_or_target() {
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
    let language = rift_syntax::ShippedLanguage::Rust.language();
    let manifest_path = ProjectPath::new("Cargo.toml").expect("manifest path");
    let root_path = ProjectPath::new("src/lib.rs").expect("library path");
    let files = [PackageSource::new(&root_path, "pub struct Example;\n")];
    for text in [
        "[package]\nname='foreign'\nversion='1.0.0'\n",
        "[package]\nname='crate-root'\nversion='2.0.0'\n",
        "[package]\nname='crate-root'\nversion='1.0.0'\n[lib]\npath='src/other.rs'\n",
        "[package]\nname='crate-root'\nversion='1.0.0'\nautolib=false\n",
        "[package",
    ] {
        let metadata = [PackageSource::new(&manifest_path, text)];
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(20, 16 * 1024 * 1024),
        )
        .expect("bounded original sources")
        .with_framework_context(&metadata, &[])
        .expect("original metadata");
        assert!(
            input.with_library(&owner, metadata[0], files[0]).is_err(),
            "manifest {text}"
        );
        assert!(input.namespace_input().library().is_none());
    }
}

#[test]
fn complete_mixed_scope_keeps_selected_library_modules_after_literal_items() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let source = "#[macro_use] mod macros; #[cfg(test)] #[macro_use] mod tests; compile_error!(\"unsupported target\"); cfg_rt! { use other::{Debug as Compass, Copy as Pointer}; #[derive(Debug, Copy, Clone, PartialEq)] enum Timer { Ready } impl Timer { fn poll<'a>(&self) { let _ = r#\"unknown! {}\"#; let _ = '\\''; } } } cfg_rt! { pub mod runtime; }";
    let selected = [
        ("src/lib.rs", source),
        ("src/macros.rs", definition),
        ("src/runtime.rs", "pub struct Runtime;"),
    ];
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("actual package owner")
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
    .expect("owner origin");
    let found = selected_anchors_for_context(
        &[("Cargo.toml", MANIFEST)],
        &selected,
        None,
        false,
        &owner,
        &origin,
        true,
    );
    assert!(found["src/runtime.rs"].contains_key("Runtime"));
    let unknown = selected_anchors_for_context(
        &[("Cargo.toml", MANIFEST)],
        &selected,
        None,
        false,
        &owner,
        &origin,
        false,
    );
    assert!(!unknown.contains_key("src/runtime.rs"));
}

#[test]
fn complete_mixed_scope_refuses_later_alias_glob_nested_and_unknown_import_effects() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("actual owner")
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
    .expect("actual origin");
    for (before, body, later) in [
        (
            "",
            "#[derive(Debug)] struct Timer;",
            "use other::Macro as Debug;",
        ),
        ("", "#[derive(Debug)] struct Timer;", "use other::*;"),
        ("", "#[derive(Debug)] struct Timer;", "unknown! {};"),
        (
            "#[macro_use] mod unknown; macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }",
            "#[derive(Debug)] struct Timer;",
            "",
        ),
        ("", "impl Timer { fn poll(&self) { unknown! {}; } }", ""),
        (
            "",
            "impl Timer { fn poll(&self) { #[macro_export] macro_rules! cfg_rt { () => {}; } } }",
            "",
        ),
        ("", "use other::Macro as cfg_rt;", ""),
    ] {
        let root = format!(
            "#[macro_use] mod macros; {before} cfg_rt! {{ {body} }} cfg_rt! {{ pub mod runtime; }} {later}"
        );
        let selected = [
            ("src/lib.rs", root.as_str()),
            ("src/macros.rs", definition),
            ("src/runtime.rs", "pub struct Runtime;"),
        ];
        let found = selected_anchors_for_context(
            &[("Cargo.toml", MANIFEST)],
            &selected,
            None,
            false,
            &owner,
            &origin,
            true,
        );
        assert!(!found.contains_key("src/runtime.rs"), "{root}");
    }
}

#[test]
#[ignore = "requires the immutable cold-run source artifact and verified Cargo manifest"]
fn cold_tokio_original_sources_keep_required_library_anchors() {
    let artifact_path = std::env::var("RIFT_COLD_TOKIO_ARTIFACT").expect("original artifact");
    let manifest_path = std::env::var("RIFT_COLD_TOKIO_MANIFEST").expect("verified manifest");
    let artifact: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact_path).expect("retained original artifact"))
            .expect("artifact data");
    let manifest =
        std::fs::read_to_string(manifest_path).expect("verified original Cargo metadata");
    let units = artifact["units"].as_array().expect("retained units");
    let selected = units
        .iter()
        .filter_map(|unit| {
            let path = unit["path"].as_str()?;
            (path.starts_with("src/macros/")
                || [
                    "src/lib.rs",
                    "src/runtime/mod.rs",
                    "src/runtime/runtime.rs",
                    "src/runtime/builder.rs",
                    "src/task/mod.rs",
                    "src/task/spawn.rs",
                ]
                .contains(&path))
            .then(|| {
                (
                    path,
                    unit["source"].as_str().expect("original complete source"),
                )
            })
        })
        .collect::<Vec<_>>();
    assert!(selected.len() < 20, "existing fixture input bound");
    let owner = serde_json::from_value(artifact["owner"].clone()).expect("actual captured owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "tokio".to_owned(),
                version: "1.53.2".to_owned(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("actual captured origin");
    let found = selected_anchors_for_context(
        &[("Cargo.toml", manifest.as_str())],
        &selected,
        None,
        false,
        &owner,
        &origin,
        true,
    );
    eprintln!(
        "actual retained input paths: {:?}",
        selected.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    eprintln!(
        "admitted namespace paths: {:?}",
        found.keys().collect::<Vec<_>>()
    );
    for (path, name) in [
        ("src/runtime/runtime.rs", "Runtime"),
        ("src/runtime/builder.rs", "Builder"),
        ("src/task/spawn.rs", "spawn"),
    ] {
        assert!(
            found
                .get(path)
                .is_some_and(|symbols| symbols.contains_key(name)),
            "required original anchor missing: {path}::{name}"
        );
    }
}

#[test]
#[ignore = "requires the immutable cold-run source artifact and verified Cargo manifest"]
fn cold_serde_original_sources_keep_required_library_anchors() {
    let artifact_path = std::env::var("RIFT_COLD_SERDE_ARTIFACT").expect("original artifact");
    let manifest_path = std::env::var("RIFT_COLD_SERDE_MANIFEST").expect("verified manifest");
    let artifact: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact_path).expect("retained original artifact"))
            .expect("artifact data");
    let manifest =
        std::fs::read_to_string(manifest_path).expect("verified original Cargo metadata");
    let units = artifact["units"].as_array().expect("retained units");
    let selected = units
        .iter()
        .filter_map(|unit| {
            let path = unit["path"].as_str()?;
            [
                "src/lib.rs",
                "src/core/lib.rs",
                "src/core/crate_root.rs",
                "src/core/macros.rs",
                "src/core/de/mod.rs",
                "src/core/de/ignored_any.rs",
                "src/core/de/impls.rs",
                "src/core/de/value.rs",
                "src/core/ser/mod.rs",
                "src/core/ser/fmt.rs",
                "src/core/ser/impls.rs",
                "src/core/ser/impossible.rs",
                "src/core/format.rs",
                "src/core/std_error.rs",
                "src/core/private/mod.rs",
                "src/core/private/doc.rs",
                "src/core/private/seed.rs",
                "src/private/mod.rs",
                "src/integer128.rs",
            ]
            .contains(&path)
            .then(|| {
                (
                    path,
                    unit["source"].as_str().expect("original complete source"),
                )
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(selected.len(), 19, "original selected source count");
    let owner = serde_json::from_value(artifact["owner"].clone()).expect("actual captured owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "serde".to_owned(),
                version: "1.0.228".to_owned(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("actual captured origin");
    let found = selected_anchors_for_context(
        &[("Cargo.toml", manifest.as_str())],
        &selected,
        None,
        false,
        &owner,
        &origin,
        true,
    );
    eprintln!(
        "actual retained input paths: {:?}",
        selected.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    eprintln!(
        "admitted namespace paths: {:?}",
        found.keys().collect::<Vec<_>>()
    );
    for (path, name) in [
        ("src/core/de/mod.rs", "Deserialize"),
        ("src/core/ser/mod.rs", "Serialize"),
    ] {
        assert!(
            found
                .get(path)
                .is_some_and(|symbols| symbols.contains_key(name)),
            "required original anchor missing: {path}::{name}"
        );
    }
}

#[test]
fn conditional_documentation_keeps_complete_selected_library_scope() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let source = "#[macro_use] mod macros; #[cfg_attr(docsrs, doc(cfg(feature = \"rt\")))] pub mod task; compile_error!(\"unsupported target\"); cfg_rt! { pub mod runtime; }";
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("actual package owner")
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
    .expect("owner origin");
    let found = selected_anchors_for_context(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", source),
            ("src/macros.rs", definition),
            ("src/runtime.rs", "pub struct Runtime;"),
        ],
        None,
        false,
        &owner,
        &origin,
        true,
    );
    let identity = SymbolIdentity::parse(found["src/runtime.rs"]["Runtime"].as_str())
        .expect("captured module identity");
    assert_eq!(identity.owner(), &owner);
    assert_eq!(
        identity.qualified_path(),
        ["crate_root", "runtime", "Runtime"]
    );
}

#[test]
fn conditional_documentation_refuses_mixed_unknown_path_and_macro_import_effects() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("actual package owner")
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
    .expect("owner origin");
    for attribute in [
        "cfg_attr(docsrs, doc(hidden), unknown())",
        "cfg_attr(docsrs, doc(hidden), path = \"other.rs\")",
        "cfg_attr(docsrs, doc(hidden), macro_use)",
        "cfg_attr(docsrs, doc(hidden), macro_use())",
        "cfg_attr(docsrs, unknown(), doc(hidden))",
    ] {
        let source = format!(
            "#[macro_use] mod macros; #[{attribute}] pub mod task; compile_error!(\"unsupported target\"); cfg_rt! {{ pub mod runtime; }}"
        );
        let found = selected_anchors_for_context(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", &source),
                ("src/macros.rs", definition),
                ("src/runtime.rs", "pub struct Runtime;"),
            ],
            None,
            false,
            &owner,
            &origin,
            true,
        );
        assert!(!found.contains_key("src/runtime.rs"), "{attribute}");
    }
}

#[test]
#[ignore = "requires the immutable cold-run source artifact and verified Cargo manifest"]
fn cold_serde_core_original_sources_keep_required_defining_anchors() {
    let artifact_path = std::env::var("RIFT_COLD_SERDE_CORE_ARTIFACT").expect("original artifact");
    let manifest_path = std::env::var("RIFT_COLD_SERDE_CORE_MANIFEST").expect("verified manifest");
    let artifact: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact_path).expect("retained original artifact"))
            .expect("artifact data");
    let manifest =
        std::fs::read_to_string(manifest_path).expect("verified original Cargo metadata");
    let units = artifact["units"].as_array().expect("retained units");
    let selected = units
        .iter()
        .filter(|unit| unit["language"].as_str() == Some("rust"))
        .map(|unit| {
            assert_eq!(unit["source_complete"].as_bool(), Some(true));
            (
                unit["path"].as_str().expect("original Rust path"),
                unit["source"]
                    .as_str()
                    .expect("original complete Rust source"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(selected.len(), 20, "complete original Rust source count");
    let owner = serde_json::from_value(artifact["owner"].clone()).expect("actual captured owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "serde_core".to_owned(),
                version: "1.0.228".to_owned(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("actual captured origin");
    let found = selected_anchors_for_context(
        &[("Cargo.toml", manifest.as_str())],
        &selected,
        None,
        false,
        &owner,
        &origin,
        true,
    );
    eprintln!(
        "actual retained input paths: {:?}",
        selected.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    eprintln!(
        "admitted namespace paths: {:?}",
        found.keys().collect::<Vec<_>>()
    );
    for (path, name) in [
        ("src/de/mod.rs", "Deserialize"),
        ("src/ser/mod.rs", "Serialize"),
    ] {
        assert!(
            found
                .get(path)
                .is_some_and(|symbols| symbols.contains_key(name)),
            "required original anchor missing: {path}::{name}"
        );
        let identity =
            SymbolIdentity::parse(found[path][name].as_str()).expect("original defining identity");
        assert_eq!(identity.owner(), &owner);
        let module = if name == "Serialize" { "ser" } else { "de" };
        assert_eq!(identity.qualified_path(), ["serde_core", module, name]);
    }
}

#[test]
fn documented_module_prefix_keeps_complete_selected_library_scope() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    for newline in ["\n", "\r\n"] {
        let source = format!(
            "#[macro_use] mod macros;{newline}/// Original module documentation.{newline}/// ```{newline}/// use crate_root::runtime::Runtime;{newline}/// ```{newline}pub mod stream {{}}{newline}cfg_rt! {{ pub mod runtime; }}"
        );
        let owner = SymbolIdentity::parse(
            "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
        )
        .expect("actual package owner")
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
        .expect("owner origin");
        let found = selected_anchors_for_context(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", &source),
                ("src/macros.rs", definition),
                ("src/runtime.rs", "pub struct Runtime;"),
            ],
            None,
            false,
            &owner,
            &origin,
            true,
        );
        let identity = SymbolIdentity::parse(found["src/runtime.rs"]["Runtime"].as_str())
            .expect("captured module identity");
        assert_eq!(identity.owner(), &owner);
        assert_eq!(
            identity.qualified_path(),
            ["crate_root", "runtime", "Runtime"]
        );
    }
}

#[test]
fn documented_module_prefix_refuses_unknown_path_and_macro_import_effects() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("actual package owner")
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
    .expect("owner origin");
    for attribute in ["unknown()", "path = \"other.rs\"", "macro_use()"] {
        let source = format!(
            "#[macro_use] mod macros; /// Original module documentation.\n#[{attribute}] pub mod stream {{}} compile_error!(\"unsupported target\"); cfg_rt! {{ pub mod runtime; }}"
        );
        let found = selected_anchors_for_context(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", &source),
                ("src/macros.rs", definition),
                ("src/runtime.rs", "pub struct Runtime;"),
            ],
            None,
            false,
            &owner,
            &origin,
            true,
        );
        assert!(!found.contains_key("src/runtime.rs"), "{attribute}");
    }
}

fn cfg_expression_anchors(prefix: &str, suffix: &str, expression: &str) -> super::super::Anchors {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature=\"rt\")] $item )* } }";
    let source = format!(
        "{prefix}#[macro_use] mod macros; cfg_rt! {{ const BOX_FUTURE_THRESHOLD: usize = if {expression} {{ 2048 }} else {{ 16384 }}; pub mod runtime; }} {suffix}"
    );
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("actual package owner")
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
    .expect("owner origin");
    selected_anchors_for_context(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", &source),
            ("src/macros.rs", definition),
            ("src/runtime.rs", "pub struct Runtime;"),
        ],
        None,
        false,
        &owner,
        &origin,
        true,
    )
}

#[test]
fn cfg_expression_preserves_unknown_predicate_and_original_const_branches() {
    for expression in [
        "cfg!(debug_assertions)",
        "cfg!(all(feature=\"rt\",not(test)))",
    ] {
        let found = cfg_expression_anchors("", "", expression);
        assert!(found["src/runtime.rs"].contains_key("Runtime"));
    }
}

#[test]
fn cfg_expression_refuses_shadow_import_unknown_effect_and_prelude_changes() {
    for (prefix, suffix) in [
        ("#![no_std]", ""),
        ("#![no_implicit_prelude]", ""),
        ("#![cfg_attr(feature=\"bare\",no_std)]", ""),
        ("use other::Macro as cfg;", ""),
        ("use other::*;", ""),
        ("macro_rules! cfg { ($($token:tt)*) => { false }; }", ""),
        ("", "unknown!{}"),
        ("", "use other::Macro as cfg;"),
    ] {
        let found = cfg_expression_anchors(prefix, suffix, "cfg!(debug_assertions)");
        assert!(!found.contains_key("src/runtime.rs"), "{prefix} {suffix}");
    }
}

#[test]
fn cfg_expression_refuses_qualified_computed_and_unknown_macro_forms() {
    for expression in [
        "other::cfg!(debug_assertions)",
        "cfg!(unknown!())",
        "unknown!(debug_assertions)",
        "cfg!{debug_assertions}",
    ] {
        let found = cfg_expression_anchors("", "", expression);
        assert!(!found.contains_key("src/runtime.rs"), "{expression}");
    }
}

#[test]
fn transparent_item_macros_validate_original_modules_before_unknown_effects() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    for (body, admitted) in [
        ("mod runtime; unknown! { mod hidden; }", true),
        ("unknown! { mod hidden; } mod runtime;", false),
        ("unknown! { mod runtime; } mod runtime;", false),
    ] {
        let source = format!("#[macro_use] mod macros; cfg_rt! {{ {body} }}");
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", source.as_str()),
                ("src/macros.rs", definition),
                ("src/runtime.rs", "pub struct Runtime;"),
                ("src/hidden.rs", "pub fn hidden() {}"),
            ],
            None,
            false,
        );
        assert_eq!(found.contains_key("src/runtime.rs"), admitted, "{source}");
        assert!(!found.contains_key("src/hidden.rs"), "{source}");
        if admitted {
            let identity = SymbolIdentity::parse(found["src/runtime.rs"]["Runtime"].as_str())
                .expect("original direct module identity");
            assert_eq!(
                identity.qualified_path(),
                ["crate_root", "runtime", "Runtime"]
            );
        }
    }
}

#[test]
fn literal_module_paths_keep_original_sources_and_logical_names() {
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            (
                "src/lib.rs",
                "#[path = \"to.rs\"] pub mod kept; mod ordinary; mod inline { #[path = \"other.rs\"] mod child; }",
            ),
            ("src/to.rs", "pub fn travelled() {}"),
            (
                "src/ordinary.rs",
                "#[path = \"same.rs\"] mod kept; mod inline { #[path = \"nested.rs\"] mod child; }",
            ),
            ("src/same.rs", "pub fn sibling() {}"),
            ("src/inline/other.rs", "pub fn from_root() {}"),
            ("src/ordinary/inline/nested.rs", "pub fn from_inline() {}"),
        ],
        None,
        false,
    );
    for (path, name, expected) in [
        (
            "src/to.rs",
            "travelled",
            vec!["crate_root", "kept", "travelled"],
        ),
        (
            "src/same.rs",
            "sibling",
            vec!["crate_root", "ordinary", "kept", "sibling"],
        ),
        (
            "src/inline/other.rs",
            "from_root",
            vec!["crate_root", "inline", "child", "from_root"],
        ),
        (
            "src/ordinary/inline/nested.rs",
            "from_inline",
            vec!["crate_root", "ordinary", "inline", "child", "from_inline"],
        ),
    ] {
        let identity = SymbolIdentity::parse(found[path][name].as_str())
            .expect("original literal module identity");
        assert_eq!(identity.qualified_path(), expected);
    }
}

#[test]
fn literal_module_paths_refuse_missing_unknown_and_conflicting_sources() {
    for root in [
        "#[path = \"missing.rs\"] mod kept;",
        "#[cfg_attr(docsrs, path = \"to.rs\")] mod kept;",
        "#[path = \"../to.rs\"] mod kept;",
        "#[path = \"directory\"] mod inline { #[path = \"to.rs\"] mod kept; }",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[("src/lib.rs", root), ("src/to.rs", "pub fn refused() {}")],
            None,
            false,
        );
        assert!(!found.contains_key("src/to.rs"), "{root}");
    }
    let conflict = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            (
                "src/lib.rs",
                "#[path = \"to.rs\"] mod first; #[path = \"to.rs\"] mod second;",
            ),
            ("src/to.rs", "pub fn refused() {}"),
        ],
        None,
        false,
    );
    assert!(
        conflict.is_empty(),
        "one physical source cannot acquire two module identities"
    );
    let ambiguous = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "mod kept;"),
            ("src/kept.rs", "pub fn first() {}"),
            ("src/kept/mod.rs", "pub fn second() {}"),
        ],
        None,
        false,
    );
    assert!(!ambiguous.contains_key("src/kept.rs"));
    assert!(!ambiguous.contains_key("src/kept/mod.rs"));
}

#[test]
fn literal_macro_function_anchors_require_original_supported_invocations() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let source = format!(
        "{definition} cfg_rt! {{ pub fn spawn<F>(future: F) -> JoinHandle<F::Output> where F: Future + Send + 'static, F::Output: Send + 'static, {{ future }} }} pub fn plain() {{}}"
    );
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[("src/lib.rs", &source)],
        None,
        false,
    );
    let identity = SymbolIdentity::parse(found["src/lib.rs"]["spawn"].as_str())
        .expect("original literal identity");
    assert_eq!(identity.qualified_path(), &["crate_root", "spawn"]);
    assert!(found["src/lib.rs"].contains_key("plain"));
}

#[test]
fn literal_macro_function_anchors_refuse_opaque_shadowed_and_unknown_effects() {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    for source in [
        "opaque! { pub fn spawn() {} } pub fn plain() {}".to_owned(),
        format!(
            "{definition} macro_rules! cfg_rt {{ () => {{}} }} cfg_rt! {{ pub fn spawn() {{}} }} pub fn plain() {{}}"
        ),
        format!(
            "{definition} cfg_rt! {{ pub fn spawn() {{ unknown! {{}} }} }} pub fn plain() {{}}"
        ),
        format!("{definition} cfg_rt! {{ #[unknown] pub fn spawn() {{}} }} pub fn plain() {{}}"),
        format!(
            "#![no_std] {definition} cfg_rt! {{ pub fn spawn() {{ let _ = cfg!(feature = \"rt\"); }} }} pub fn plain() {{}}"
        ),
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[("src/lib.rs", &source)],
            None,
            false,
        );
        assert!(!found["src/lib.rs"].contains_key("spawn"), "{source}");
        assert!(found["src/lib.rs"].contains_key("plain"), "{source}");
    }
}

fn function_body_anchors(
    prefix: &str,
    body: &str,
    suffix: &str,
    library: bool,
) -> super::super::Anchors {
    let definition =
        "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }";
    let source = format!(
        "{prefix}{definition} cfg_rt! {{ pub fn spawn<F>(future: F) -> JoinHandle<F::Output> where F: Future + Send + 'static, F::Output: Send + 'static, {{ {body} }} }} {suffix} pub fn plain() {{}}"
    );
    let owner = SymbolIdentity::parse(
        "rift://symbol/cargo/crates.io/crate-root@1.0.0/rust/crate_root/Example",
    )
    .expect("actual package owner")
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
    .expect("owner origin");
    selected_anchors_for_context(
        &[("Cargo.toml", MANIFEST)],
        &[("src/lib.rs", &source)],
        None,
        false,
        &owner,
        &origin,
        library,
    )
}

#[test]
fn literal_function_bodies_keep_local_imports_and_checked_panic() {
    for body in [
        "use crate::runtime::{context, task}; let future = task::spawn(future); match context::with_current(future) { Ok(value) => value, Err(e) => panic!(\"{}\", e), }",
        "use crate::runtime::context as local_context; { use crate::runtime::task; } panic!(\"{}\", local_context)",
    ] {
        let found = function_body_anchors("", body, "", true);
        let identity = SymbolIdentity::parse(found["src/lib.rs"]["spawn"].as_str())
            .expect("original function identity");
        assert_eq!(identity.qualified_path(), &["crate_root", "spawn"]);
        assert!(found["src/lib.rs"].contains_key("plain"));
    }
    let found = function_body_anchors(
        "pub fn borrowed(value: &Value) -> &Value { value }",
        "future",
        "#[track_caller] pub(super) fn spawn_inner<T>(future: T, meta: SpawnMeta<'_>) -> JoinHandle<T::Output> where T: Future + Send + 'static, T::Output: Send + 'static, { use crate::runtime::{context, task}; panic!(\"{}\", context); }",
        true,
    );
    assert!(found["src/lib.rs"].contains_key("spawn"));
    let found = function_body_anchors("", "use crate::runtime::{context, task}; future", "", false);
    assert!(found["src/lib.rs"].contains_key("spawn"));
}

#[test]
fn literal_function_bodies_keep_local_imports_out_of_outer_scope() {
    for body in [
        "use crate::other::Copy; future",
        "use crate::other::Macro as Copy; future",
    ] {
        let found =
            function_body_anchors("", body, "#[derive(Copy, Clone)] pub struct Marker;", true);
        assert!(found["src/lib.rs"].contains_key("spawn"), "{body}");
        assert!(found["src/lib.rs"].contains_key("Marker"), "{body}");
    }
}

#[test]
fn literal_function_bodies_refuse_panic_shadow_and_missing_prelude() {
    for (prefix, body, suffix, library) in [
        (
            "",
            "use crate::other::Macro as panic; panic!(\"message\");",
            "",
            true,
        ),
        (
            "",
            "panic!(\"message\"); use crate::other::Macro as panic;",
            "",
            true,
        ),
        ("", "use crate::other::*; panic!(\"message\");", "", true),
        (
            "use crate::other::Macro as panic;",
            "panic!(\"message\");",
            "",
            true,
        ),
        (
            "",
            "panic!(\"message\");",
            "use crate::other::Macro as panic;",
            true,
        ),
        ("#![no_std]", "panic!(\"message\");", "", true),
        ("#![no_implicit_prelude]", "panic!(\"message\");", "", true),
        (
            "#![cfg_attr(feature=\"bare\",no_std)]",
            "panic!(\"message\");",
            "",
            true,
        ),
        ("", "panic!(\"message\");", "", false),
        ("", "other::panic!(\"message\");", "", true),
    ] {
        let found = function_body_anchors(prefix, body, suffix, library);
        assert!(
            !found["src/lib.rs"].contains_key("spawn"),
            "{prefix} {body} {suffix}"
        );
        assert!(found["src/lib.rs"].contains_key("plain"));
    }
}

#[test]
fn literal_function_bodies_refuse_unknown_and_exported_macro_effects() {
    for (body, suffix) in [
        ("use crate::runtime::context; unknown! {}; future", ""),
        ("panic!(unknown!());", ""),
        ("macro_rules! panic { () => {} } panic!();", ""),
        (
            "#[macro_export] macro_rules! emitted { () => {} } future",
            "",
        ),
        ("#[unknown] let value = future; value", ""),
        ("extern crate other; future", ""),
        ("mod emitted; future", ""),
        ("panic!(\"message\");", "unknown! {};"),
    ] {
        let found = function_body_anchors("", body, suffix, true);
        assert!(
            !found["src/lib.rs"].contains_key("spawn"),
            "{body} {suffix}"
        );
        assert!(found["src/lib.rs"].contains_key("plain"));
    }
    let source = "opaque! { pub fn spawn() { use crate::runtime::context; panic!(\"message\"); } } pub fn plain() {}";
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[("src/lib.rs", source)],
        None,
        false,
    );
    assert!(!found["src/lib.rs"].contains_key("spawn"));
    assert!(found["src/lib.rs"].contains_key("plain"));
}

#[test]
fn literal_empty_matcher_definitions_keep_captured_conditional_declaration_sources() {
    let definition = "macro_rules! crate_root { () => { #[cfg_attr(all(docsrs, if_docsrs_then_no_serde_core), path=\"core/de/mod.rs\")] pub mod de; pub mod ser; include!(concat!(env!(\"OUT_DIR\"), \"/private.rs\")); }; }";
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", "#[macro_use] mod crate_root; crate_root!();"),
            ("src/crate_root.rs", definition),
            ("src/de/mod.rs", "pub trait Deserialize {}"),
            ("src/ser/mod.rs", "pub trait Serialize {}"),
        ],
        None,
        false,
    );
    for (path, name, module) in [
        ("src/de/mod.rs", "Deserialize", "de"),
        ("src/ser/mod.rs", "Serialize", "ser"),
    ] {
        let identity = SymbolIdentity::parse(found[path][name].as_str())
            .expect("original conditional declaration identity");
        assert_eq!(identity.qualified_path(), ["crate_root", module, name]);
    }
}

#[test]
fn literal_empty_matcher_modules_keep_original_rows_before_and_after_unknown_effects() {
    for definition in [
        "macro_rules! crate_root { () => { pub mod de; unknown! {}; pub mod ser; }; }",
        "macro_rules! crate_root { () => { include!(\"generated.rs\"); pub mod de; pub mod ser; }; }",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", "#[macro_use] mod crate_root; crate_root!();"),
                ("src/crate_root.rs", definition),
                ("src/de.rs", "pub trait Deserialize {}"),
                ("src/ser.rs", "pub trait Serialize {}"),
            ],
            None,
            false,
        );
        assert!(found["src/de.rs"].contains_key("Deserialize"));
        assert!(found["src/ser.rs"].contains_key("Serialize"));
    }
}

#[test]
fn literal_empty_matcher_modules_refuse_missing_conflicting_and_duplicate_sources() {
    let root = "#[macro_use] mod crate_root; crate_root!();";
    for definition in [
        "macro_rules! crate_root { () => { #[path=\"missing.rs\"] pub mod de; }; }",
        "macro_rules! crate_root { () => { pub mod de; pub mod de; }; }",
        "macro_rules! crate_root { () => { #[cfg_attr(docsrs, path=\"other.rs\")] pub mod de; }; }",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", root),
                ("src/crate_root.rs", definition),
                ("src/de.rs", "pub trait Deserialize {}"),
                ("src/other.rs", "pub trait Other {}"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/de.rs"), "{definition}");
        assert!(!found.contains_key("src/other.rs"), "{definition}");
    }
    let found = selected_anchors(
        &[("Cargo.toml", MANIFEST)],
        &[
            ("src/lib.rs", root),
            (
                "src/crate_root.rs",
                "macro_rules! crate_root { () => { pub mod de; }; }",
            ),
            ("src/de.rs", "pub trait First {}"),
            ("src/de/mod.rs", "pub trait Second {}"),
        ],
        None,
        false,
    );
    assert!(!found.contains_key("src/de.rs"));
    assert!(!found.contains_key("src/de/mod.rs"));
}

#[test]
fn literal_empty_matcher_modules_refuse_unknown_matchers_arguments_and_shadowed_definitions() {
    let definition = "macro_rules! crate_root { () => { pub mod de; }; }";
    for root in [
        "#[macro_use] mod crate_root; crate_root!(argument);",
        "#[macro_use] mod crate_root; use other::Macro as crate_root; crate_root!();",
        "#[macro_use] mod crate_root; unknown! {}; crate_root!();",
        "#[macro_use] mod crate_root; macro_rules! crate_root { () => {}; } crate_root!();",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", root),
                ("src/crate_root.rs", definition),
                ("src/de.rs", "pub trait Refused {}"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/de.rs"), "{root}");
    }
    for definition in [
        "macro_rules! crate_root { ($value:expr) => { pub mod de; }; }",
        "macro_rules! crate_root { () => { pub mod de; }; () => { pub mod other; }; }",
        "macro_rules! crate_root { () => { #[cfg_attr(docsrs, custom)] pub mod de; }; }",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", "#[macro_use] mod crate_root; crate_root!();"),
                ("src/crate_root.rs", definition),
                ("src/de.rs", "pub trait Refused {}"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/de.rs"), "{definition}");
    }
}

#[test]
fn literal_empty_matcher_modules_keep_references_after_opaque_exported_definitions() {
    for attribute in ["#[macro_export]", "#[macro_export(local_inner_macros)]"] {
        let macros = format!(
            "{attribute} macro_rules! helper {{ ($($tokens:tt)*) => {{ unknown! {{ $($tokens)* }} }}; }}"
        );
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                (
                    "src/lib.rs",
                    "#[macro_use] mod crate_root; #[macro_use] mod macros; crate_root!();",
                ),
                (
                    "src/crate_root.rs",
                    "macro_rules! crate_root { () => { pub mod de; }; }",
                ),
                ("src/macros.rs", macros.as_str()),
                ("src/de.rs", "pub trait Deserialize {}"),
            ],
            None,
            false,
        );
        let identity = SymbolIdentity::parse(found["src/de.rs"]["Deserialize"].as_str())
            .expect("original declaration after opaque definition");
        assert_eq!(
            identity.qualified_path(),
            ["crate_root", "de", "Deserialize"]
        );
    }
}

#[test]
fn literal_empty_matcher_modules_refuse_exported_definition_invocations() {
    for attribute in ["#[macro_export]", "#[macro_export(local_inner_macros)]"] {
        let definition =
            format!("{attribute} macro_rules! crate_root {{ () => {{ pub mod de; }}; }}");
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                ("src/lib.rs", "#[macro_use] mod crate_root; crate_root!();"),
                ("src/crate_root.rs", definition.as_str()),
                ("src/de.rs", "pub trait Deserialize {}"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/de.rs"), "{definition}");
    }
}

#[test]
fn literal_empty_matcher_modules_refuse_unknown_and_misattached_export_attributes() {
    for macros in [
        "#[macro_export(unknown)] macro_rules! helper { () => {}; }",
        "#[macro_export(local_inner_macros)] pub fn helper() {}",
        "#[macro_export(local_inner_macros, unknown)] macro_rules! helper { () => {}; }",
        "#[macro_use] mod unknown; #[macro_export(local_inner_macros)] macro_rules! helper { () => {}; }",
        "#[macro_export(local_inner_macros)] macro_rules! helper { () => {}; } helper!();",
    ] {
        let found = selected_anchors(
            &[("Cargo.toml", MANIFEST)],
            &[
                (
                    "src/lib.rs",
                    "#[macro_use] mod crate_root; #[macro_use] mod macros; crate_root!();",
                ),
                (
                    "src/crate_root.rs",
                    "macro_rules! crate_root { () => { pub mod de; }; }",
                ),
                ("src/macros.rs", macros),
                ("src/de.rs", "pub trait Deserialize {}"),
            ],
            None,
            false,
        );
        assert!(!found.contains_key("src/de.rs"), "{macros}");
    }
}

fn local_alias_analysis(selected: &[(&str, &str)], manifest: &str) -> crate::PackageAnalysis {
    let package = PackageIdentity {
        manager: "cargo".to_owned(),
        registry: "crates.io".to_owned(),
        name: "serde_core".to_owned(),
        version: "1.0.228".to_owned(),
    };
    let owner = package.owner().expect("captured package owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("captured origin");
    let language = rift_syntax::ShippedLanguage::Rust.language();
    let paths = selected
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("original path"))
        .collect::<Vec<_>>();
    let files = paths
        .iter()
        .zip(selected)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let manifest_path = ProjectPath::new("Cargo.toml").expect("manifest path");
    let metadata = PackageSource::new(&manifest_path, manifest);
    let context = [metadata];
    let root = files
        .iter()
        .find(|file| file.path().as_str() == "src/lib.rs")
        .copied()
        .expect("original root");
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(21, 16 * 1024 * 1024),
    )
    .expect("bounded package")
    .with_framework_context(&context, &[])
    .expect("original manifest")
    .with_library(&owner, metadata, root)
    .expect("selected non-test library");
    crate::PackageAnalyzer::analyze(input, 1).expect("checked original publication")
}

const LOCAL_ALIAS_MANIFEST: &str =
    "[package]\nname='serde_core'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n";
const LOCAL_ALIAS_ROOT: &str = "#[macro_use]\nmod crate_root;\ncrate_root!();\n";
const LOCAL_ALIAS_DEFINITION: &str = "macro_rules! crate_root { () => { pub mod de; pub mod ser; #[doc(inline)] pub use crate::de::{Deserialize, Deserializer}; #[doc(inline)] pub use crate::ser::{Serialize, Serializer}; include!(concat!(env!(\"OUT_DIR\"), \"/private.rs\")); }; }\n";

fn local_alias_sources(definition: &str) -> Vec<(&str, &str)> {
    vec![
        ("src/lib.rs", LOCAL_ALIAS_ROOT),
        ("src/crate_root.rs", definition),
        (
            "src/de/mod.rs",
            "pub trait Deserialize {}\npub trait Deserializer {}\n",
        ),
        (
            "src/ser/mod.rs",
            "pub trait Serialize {}\npub trait Serializer {}\n",
        ),
    ]
}

fn assert_local_alias(
    publication: &rift_protocol::index::PackagePublication,
    name: &str,
    module: &str,
) {
    let alias = format!("rift://symbol/cargo/crates.io/serde_core@1.0.228/rust/serde_core/{name}");
    let target =
        format!("rift://symbol/cargo/crates.io/serde_core@1.0.228/rust/serde_core/{module}/{name}");
    let object = publication
        .objects
        .iter()
        .find(|object| object.id.as_ref().is_some_and(|id| id.as_str() == alias))
        .expect("source-backed public alias");
    assert!(
        object
            .facets
            .contains(&rift_protocol::read::SymbolFacet::Alias)
    );
    let binding = publication
        .declarations
        .iter()
        .find(|declaration| Some(&declaration.symbol) == object.id.as_ref())
        .expect("original public binding");
    assert!(binding.public && binding.source_complete);
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.unit == binding.unit)
        .expect("original defining unit");
    assert_eq!(unit.path.0, "src/crate_root.rs");
    assert_eq!(
        unit.source.get(
            usize::try_from(binding.range.start).expect("start")
                ..usize::try_from(binding.range.end).expect("end")
        ),
        Some(binding.source.as_str())
    );
    assert!(binding.source.contains("pub use crate::"));
    let identity = SymbolIdentity::parse(object.id.as_ref().expect("canonical alias").as_str())
        .expect("owner-preserving identity");
    assert_eq!(identity.owner(), &publication.owner);
    let edge = publication
        .relationships
        .iter()
        .find(|edge| {
            Some(&edge.from) == object.id.as_ref()
                && edge.kind.0 == rift_protocol::read::RelationshipFacet::Aliases.as_ref()
        })
        .expect("explicit alias target");
    assert_eq!(edge.to.as_str(), target);
    assert_eq!(
        edge.occurrence
            .as_ref()
            .expect("original statement occurrence")
            .unit,
        binding.unit
    );
    assert_eq!(
        edge.occurrence
            .as_ref()
            .expect("original statement occurrence")
            .range,
        binding.range
    );
    assert!(publication.objects.iter().any(|object| {
        object.id.as_ref().is_some_and(|id| id.as_str() == target)
            && object
                .facets
                .contains(&rift_protocol::read::SymbolFacet::Type)
    }));
}

#[test]
fn literal_root_public_reexports_keep_original_defining_traits() {
    let sources = local_alias_sources(LOCAL_ALIAS_DEFINITION);
    let analysis = local_alias_analysis(&sources, LOCAL_ALIAS_MANIFEST);
    for (name, module) in [
        ("Deserialize", "de"),
        ("Deserializer", "de"),
        ("Serialize", "ser"),
        ("Serializer", "ser"),
    ] {
        assert_local_alias(analysis.publication(), name, module);
    }
    for (path, source) in sources {
        let unit = analysis
            .publication()
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("original unit");
        assert_eq!(unit.source, source);
        assert!(unit.source_complete);
    }
}

#[test]
fn literal_root_public_reexports_refuse_missing_and_conflicting_targets() {
    let mut sources = local_alias_sources(LOCAL_ALIAS_DEFINITION);
    sources.retain(|(path, _)| *path != "src/de/mod.rs");
    let analysis = local_alias_analysis(&sources, LOCAL_ALIAS_MANIFEST);
    assert!(!analysis.publication().objects.iter().any(|object| {
        object
            .id
            .as_ref()
            .is_some_and(|id| id.as_str().ends_with("/serde_core/Deserialize"))
    }));
    assert_local_alias(analysis.publication(), "Serialize", "ser");
    let conflicting = "macro_rules! crate_root { () => { pub mod de; pub mod ser; pub use crate::de::Deserialize as Shared; pub use crate::ser::Serialize as Shared; }; }\n";
    let sources = local_alias_sources(conflicting);
    let analysis = local_alias_analysis(&sources, LOCAL_ALIAS_MANIFEST);
    assert!(!analysis.publication().objects.iter().any(|object| {
        object
            .id
            .as_ref()
            .is_some_and(|id| id.as_str().ends_with("/serde_core/Shared"))
    }));
    assert!(analysis.publication().objects.iter().any(|object| {
        object
            .id
            .as_ref()
            .is_some_and(|id| id.as_str().ends_with("/serde_core/ser/Serialize"))
    }));
}

#[test]
fn literal_root_public_reexports_refuse_opaque_and_unknown_attributes() {
    for definition in [
        "macro_rules! crate_root { ($item:item) => { pub mod ser; pub use crate::ser::Serialize; }; }\n",
        "macro_rules! crate_root { () => { pub mod ser; #[unknown] pub use crate::ser::Serialize; }; }\n",
        "macro_rules! crate_root { () => { pub mod ser; pub(crate) use crate::ser::Serialize; }; }\n",
    ] {
        let sources = local_alias_sources(definition);
        let analysis = local_alias_analysis(&sources, LOCAL_ALIAS_MANIFEST);
        assert!(
            !analysis.publication().objects.iter().any(|object| object
                .id
                .as_ref()
                .is_some_and(|id| id.as_str().ends_with("/serde_core/Serialize"))),
            "opaque or unattested binding: {definition}"
        );
    }
}

#[test]
#[ignore = "requires immutable original serde_core source artifact and verified manifest"]
fn cold_serde_core_original_public_reexports_keep_defining_traits() {
    let artifact_path = std::env::var("RIFT_COLD_SERDE_CORE_ARTIFACT").expect("original artifact");
    let manifest_path = std::env::var("RIFT_COLD_SERDE_CORE_MANIFEST").expect("verified manifest");
    let artifact: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact_path).expect("immutable original bytes"))
            .expect("original artifact");
    let manifest = std::fs::read_to_string(manifest_path).expect("verified original manifest");
    let selected = artifact["units"]
        .as_array()
        .expect("original units")
        .iter()
        .filter(|unit| unit["language"].as_str() == Some("rust"))
        .map(|unit| {
            assert_eq!(unit["source_complete"].as_bool(), Some(true));
            (
                unit["path"].as_str().expect("original path"),
                unit["source"].as_str().expect("original source"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(selected.len(), 20);
    let analysis = local_alias_analysis(&selected, &manifest);
    assert_eq!(
        serde_json::to_value(&analysis.publication().owner).expect("owner"),
        artifact["owner"]
    );
    for (name, module) in [
        ("Deserialize", "de"),
        ("Deserializer", "de"),
        ("Serialize", "ser"),
        ("Serializer", "ser"),
    ] {
        assert_local_alias(analysis.publication(), name, module);
    }
}

fn external_alias_analysis(
    target: &rift_protocol::index::PackagePublication,
    selected: &[(&str, &str)],
    manifest: &str,
    include_target: bool,
) -> crate::PackageAnalysis {
    let package = PackageIdentity {
        manager: "cargo".to_owned(),
        registry: "crates.io".to_owned(),
        name: "serde".to_owned(),
        version: "1.0.228".to_owned(),
    };
    let owner = package.owner().expect("source owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("source origin");
    let language = rift_syntax::ShippedLanguage::Rust.language();
    let paths = selected
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("original path"))
        .collect::<Vec<_>>();
    let files = paths
        .iter()
        .zip(selected)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let manifest_path = ProjectPath::new("Cargo.toml").expect("manifest path");
    let metadata = PackageSource::new(&manifest_path, manifest);
    let context = [metadata];
    let root = files
        .iter()
        .find(|file| file.path().as_str() == "src/lib.rs")
        .copied()
        .expect("root");
    let rift_protocol::identity::SymbolOwner::Package {
        manager,
        registry,
        name,
        version,
    } = &target.owner
    else {
        panic!("released package target");
    };
    let target_origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: manager.clone(),
                registry: registry.clone(),
                name: name.clone(),
                version: version.clone(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("target origin");
    let target_paths = target
        .units
        .iter()
        .map(|unit| ProjectPath::new(unit.path.0.as_str()).expect("admitted target path"))
        .collect::<Vec<_>>();
    let target_units = target
        .units
        .iter()
        .map(|unit| {
            rift_core::SourceUnitId::parse(unit.unit.as_str()).expect("admitted target unit")
        })
        .collect::<Vec<_>>();
    let target_sources = target
        .units
        .iter()
        .zip(&target_units)
        .zip(&target_paths)
        .map(|((unit, source_unit), path)| {
            PackageSource::new(path, &unit.source)
                .with_source_unit(source_unit, &target_origin)
                .expect("original target source")
        })
        .collect::<Vec<_>>();
    let dependencies = [crate::PackageDependency::new(
        "serde_core",
        metadata,
        target,
        &target_sources,
    )];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(128, 32 * 1024 * 1024),
    )
    .expect("bounded source")
    .with_framework_context(&context, &[])
    .expect("source manifest")
    .with_library(&owner, metadata, root)
    .expect("source library");
    let input = if include_target {
        input
            .with_dependencies(&dependencies)
            .expect("admitted dependency")
    } else {
        input
    };
    crate::PackageAnalyzer::analyze(input, 1).expect("checked external binding")
}

fn assert_external_alias(publication: &rift_protocol::index::PackagePublication, name: &str) {
    let alias = format!("rift://symbol/cargo/crates.io/serde@1.0.228/rust/serde/{name}");
    let target = format!("rift://symbol/cargo/crates.io/serde_core@1.0.228/rust/serde_core/{name}");
    let object = publication
        .objects
        .iter()
        .find(|object| object.id.as_ref().is_some_and(|id| id.as_str() == alias))
        .expect("original public reexport");
    assert!(
        object
            .facets
            .contains(&rift_protocol::read::SymbolFacet::Alias)
    );
    assert_eq!(
        SymbolIdentity::parse(object.id.as_ref().expect("alias identity").as_str())
            .expect("source owner")
            .owner(),
        &publication.owner
    );
    let binding = publication
        .declarations
        .iter()
        .find(|row| Some(&row.symbol) == object.id.as_ref())
        .expect("source binding");
    assert!(binding.public && binding.source_complete);
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.unit == binding.unit)
        .expect("source unit");
    assert_eq!(
        unit.source.get(
            usize::try_from(binding.range.start).expect("start")
                ..usize::try_from(binding.range.end).expect("end")
        ),
        Some(binding.source.as_str())
    );
    assert!(binding.source.contains("pub use serde_core"));
    let edge = publication
        .relationships
        .iter()
        .find(|edge| {
            Some(&edge.from) == object.id.as_ref()
                && edge.kind.0 == rift_protocol::read::RelationshipFacet::Aliases.as_ref()
        })
        .expect("explicit target binding");
    assert_eq!(edge.to.as_str(), target);
    assert_eq!(
        edge.occurrence.as_ref().expect("original occurrence").unit,
        binding.unit
    );
    assert_eq!(
        edge.occurrence.as_ref().expect("original occurrence").range,
        binding.range
    );
    assert!(
        !publication
            .objects
            .iter()
            .any(|object| object.id.as_ref().is_some_and(|id| id.as_str() == target))
    );
    assert!(
        !publication
            .units
            .iter()
            .any(|unit| unit.unit.as_str().contains("serde_core@"))
    );
}

#[test]
fn external_public_reexports_keep_original_alias_chain_and_defining_owner() {
    let target = local_alias_analysis(
        &[
            (
                "src/lib.rs",
                "pub mod ser;\npub use crate::ser::Serialize;\n",
            ),
            ("src/ser.rs", "pub trait Serialize {}\n"),
        ],
        "[package]\nname='serde_core'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n",
    );
    let source = external_alias_analysis(
        target.publication(),
        &[("src/lib.rs", "pub use serde_core::Serialize;\n")],
        "[package]\nname='serde'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde_core='=1.0.228'\n",
        true,
    );
    assert_external_alias(source.publication(), "Serialize");
    assert!(
        target.publication().relationships.iter().any(|edge| edge
            .from
            .as_str()
            .ends_with("/serde_core/Serialize")
            && edge.to.as_str().ends_with("/serde_core/ser/Serialize"))
    );
}

#[test]
fn external_public_reexports_without_admitted_target_remain_unresolved() {
    let target = local_alias_analysis(
        &[("src/lib.rs", "pub trait Serialize {}\n")],
        "[package]\nname='serde_core'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n",
    );
    let source = external_alias_analysis(
        target.publication(),
        &[("src/lib.rs", "pub use serde_core::Serialize;\n")],
        "[package]\nname='serde'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde_core='=1.0.228'\n",
        false,
    );
    assert!(!source.publication().objects.iter().any(|object| {
        object
            .facets
            .contains(&rift_protocol::read::SymbolFacet::Alias)
    }));
}

#[test]
fn external_public_reexports_refuse_missing_target_and_opaque_source() {
    let target = local_alias_analysis(
        &[("src/lib.rs", "pub trait Other {}\n")],
        "[package]\nname='serde_core'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n",
    );
    for text in [
        "pub use serde_core::Serialize;\n",
        "unknown! { pub use serde_core::Other; }\n",
    ] {
        let source = external_alias_analysis(
            target.publication(),
            &[("src/lib.rs", text)],
            "[package]\nname='serde'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde_core='=1.0.228'\n",
            true,
        );
        assert!(!source.publication().objects.iter().any(|object| {
            object
                .facets
                .contains(&rift_protocol::read::SymbolFacet::Alias)
        }));
    }
}

#[test]
#[ignore = "requires immutable original Serde and serde_core artifacts and verified manifests"]
fn cold_serde_original_public_reexports_keep_serde_core_owner() {
    let core_artifact: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::env::var("RIFT_COLD_SERDE_CORE_ARTIFACT").expect("target artifact"))
            .expect("original target bytes"),
    )
    .expect("target artifact");
    let core_manifest = std::fs::read_to_string(
        std::env::var("RIFT_COLD_SERDE_CORE_MANIFEST").expect("target manifest"),
    )
    .expect("original target manifest");
    let target_files = core_artifact["units"]
        .as_array()
        .expect("target units")
        .iter()
        .filter(|unit| unit["language"].as_str() == Some("rust"))
        .map(|unit| {
            assert_eq!(unit["source_complete"].as_bool(), Some(true));
            (
                unit["path"].as_str().expect("target path"),
                unit["source"].as_str().expect("target source"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(target_files.len(), 20);
    let target = local_alias_analysis(&target_files, &core_manifest);
    for (name, module) in [("Deserialize", "de"), ("Serialize", "ser")] {
        assert_local_alias(target.publication(), name, module);
    }
    let artifact: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::env::var("RIFT_COLD_SERDE_ARTIFACT").expect("source artifact"))
            .expect("original source bytes"),
    )
    .expect("source artifact");
    let manifest = std::fs::read_to_string(
        std::env::var("RIFT_COLD_SERDE_MANIFEST").expect("source manifest"),
    )
    .expect("original source manifest");
    let selected = artifact["units"]
        .as_array()
        .expect("source units")
        .iter()
        .filter(|unit| unit["language"].as_str() == Some("rust"))
        .map(|unit| {
            assert_eq!(unit["source_complete"].as_bool(), Some(true));
            (
                unit["path"].as_str().expect("source path"),
                unit["source"].as_str().expect("source bytes"),
            )
        })
        .collect::<Vec<_>>();
    let source = external_alias_analysis(target.publication(), &selected, &manifest, true);
    assert_eq!(
        serde_json::to_value(&source.publication().owner).expect("source owner"),
        artifact["owner"]
    );
    for name in ["Deserialize", "Serialize"] {
        assert_external_alias(source.publication(), name);
    }
}
