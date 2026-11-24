//! Static package facts selected by a captured Rust build configuration.
#![cfg(feature = "collector")]

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource, RustTarget,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::read::{EdgeKind, PackageIdentity};
use rift_syntax::ShippedLanguage;

#[test]
fn test_rust_captured_configuration_selects_stored_facts_without_guessing() {
    // #825: a complete selected build distinguishes absent flags from unknown flags.
    let package = PackageIdentity {
        manager: "cargo".into(),
        registry: "crates.io".into(),
        name: "fixture".into(),
        version: "0.1.0".into(),
    };
    let owner = package.owner().expect("owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("origin");
    let language = ShippedLanguage::Rust.language();
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let source = "pub struct Item { #[cfg(not(custom))] pub value: usize } impl Item { pub fn read(&self) -> usize { #[cfg(not(custom))] { self.value } #[cfg(custom)] { 0 } } } #[cfg(not(custom))] pub fn leaf() -> usize { 1 } pub fn run() -> usize { #[cfg(not(custom))] { leaf() } #[cfg(custom)] { 0 } }";
    let files = [PackageSource::new(&path, source)];
    let context = [PackageSource::new(
        &manifest,
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )];
    let absent: &[&str] = &[];
    let present: &[&str] = &["custom"];
    for (flags, selected) in [(None, false), (Some(absent), true), (Some(present), false)] {
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(10, 65_536),
        )
        .expect("input")
        .with_framework_context(&context, &[])
        .expect("context");
        let input = match flags {
            Some(flags) => input
                .with_rust_configuration(&manifest, flags)
                .expect("captured flags"),
            None => input,
        };
        let analysis = PackageAnalyzer::analyze(input, 1).expect("analysis");
        assert!(
            analysis
                .publication()
                .objects
                .iter()
                .all(|object| object.id.is_some()),
            "every declaration keeps its identity"
        );
        for (kind, from, to) in [
            (EdgeKind::Calls, "/fixture/run", "/fixture/leaf"),
            (EdgeKind::Reads, "/Item/read", "/Item/value"),
            (EdgeKind::Contains, "/fixture/Item", "/Item/value"),
        ] {
            let edges = analysis
                .publication()
                .edges
                .iter()
                .filter(|edge| {
                    edge.kinds.contains(&kind)
                        && edge.from.0.ends_with(from)
                        && edge.to.0.ends_with(to)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                !edges.is_empty(),
                selected,
                "{flags:?}: {kind:?} {from} -> {to}"
            );
            for edge in edges {
                let occurrence = edge.occurrence.as_ref().expect("source occurrence");
                let text = &source[usize::try_from(occurrence.range.start).expect("start")
                    ..usize::try_from(occurrence.range.end).expect("end")];
                let expected = match kind {
                    EdgeKind::Calls => "leaf()",
                    EdgeKind::Reads => "self.value",
                    EdgeKind::Contains => "value",
                    _ => unreachable!("fixture kind"),
                };
                assert_eq!(text, expected);
            }
        }
        let syntax = analysis
            .files()
            .iter()
            .find(|file| file.file().path() == &path)
            .expect("source")
            .file()
            .syntax();
        let conditions = syntax
            .static_facts()
            .expect("static facts")
            .conditional_items();
        assert_eq!(conditions.len(), 6);
        assert!(conditions.iter().all(|item| {
            source[usize::try_from(item.attribute.start).expect("start")
                ..usize::try_from(item.attribute.end).expect("end")]
                .starts_with("#[cfg(")
        }));
    }
}

#[test]
fn test_rust_captured_configuration_refuses_invalid_flags_and_moved_metadata() {
    let package = PackageIdentity {
        manager: "cargo".into(),
        registry: "crates.io".into(),
        name: "fixture".into(),
        version: "0.1.0".into(),
    };
    let owner = package.owner().expect("owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("origin");
    let language = ShippedLanguage::Rust.language();
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let source = "pub fn run() {}";
    let files = [PackageSource::new(&path, source)];
    let context = [PackageSource::new(
        &manifest,
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )];
    let absent: &[&str] = &[];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(10, 65_536),
    )
    .expect("input")
    .with_framework_context(&context, &[])
    .expect("context");
    let other = ProjectPath::new("other/Cargo.toml").expect("other manifest");
    assert!(input.with_rust_configuration(&other, absent).is_err());
    assert!(
        input
            .with_rust_configuration(&manifest, &["custom() "])
            .is_err()
    );
    let configured = input
        .with_rust_configuration(&manifest, absent)
        .expect("captured flags");
    assert!(configured.with_framework_context(&context, &[]).is_ok());
    assert!(
        configured.with_framework_context(&[], &[]).is_err(),
        "moving metadata must refuse the old selected build configuration"
    );
}

/// Items under feature, target and mixed conditions, each calling and reading the same facts.
const TARGET_SOURCE: &str = "pub struct Item { pub value: usize }\npub fn leaf() -> usize { 1 }\nimpl Item {\n    #[cfg(feature = \"std\")]\n    pub fn by_feature(&self) -> usize { leaf() + self.value }\n    #[cfg(unix)]\n    pub fn by_unix(&self) -> usize { leaf() + self.value }\n    #[cfg(any(unix, windows, target_os = \"wasi\"))]\n    pub fn by_any_target(&self) -> usize { leaf() + self.value }\n    #[cfg(all(feature = \"std\", any(unix, windows)))]\n    pub fn by_feature_and_target(&self) -> usize { leaf() + self.value }\n    #[cfg(not(target_os = \"emscripten\"))]\n    pub fn by_not_target(&self) -> usize { leaf() + self.value }\n    #[cfg(windows)]\n    pub fn by_windows(&self) -> usize { leaf() + self.value }\n}\n";

/// A unix and a windows build configuration, both with the default `std` feature.
const UNIX: &[&str] = &[
    "unix",
    "target_family=\"unix\"",
    "target_os=\"linux\"",
    "feature=\"default\"",
    "feature=\"std\"",
];
const WINDOWS: &[&str] = &[
    "windows",
    "target_family=\"windows\"",
    "target_os=\"windows\"",
    "feature=\"default\"",
    "feature=\"std\"",
];

/// The conditional methods a unix and a windows build admit.
const UNIX_METHODS: &[&str] = &[
    "by_feature",
    "by_unix",
    "by_any_target",
    "by_feature_and_target",
    "by_not_target",
];
const WINDOWS_METHODS: &[&str] = &[
    "by_feature",
    "by_any_target",
    "by_feature_and_target",
    "by_not_target",
    "by_windows",
];

#[test]
fn test_rust_target_conditions_follow_the_selected_target() {
    // #859: `unix`, `windows` and `target_os` predicates are decided by the selected target
    // or the captured build configuration, the same way on every host. A package without
    // either uses one fixed target. Without a target the declarations keep their identities
    // and no fact inside them is resolved.
    let package = PackageIdentity {
        manager: "cargo".into(),
        registry: "crates.io".into(),
        name: "fixture".into(),
        version: "0.1.0".into(),
    };
    let owner = package.owner().expect("owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("origin");
    let language = ShippedLanguage::Rust.language();
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let files = [PackageSource::new(&path, TARGET_SOURCE)];
    let context = [PackageSource::new(
        &manifest,
        "[package]\nname='fixture'\nversion='0.1.0'\n[features]\ndefault=['std']\nstd=[]\n",
    )];
    let methods = [
        "by_feature",
        "by_unix",
        "by_any_target",
        "by_feature_and_target",
        "by_not_target",
        "by_windows",
    ];
    let linux_target = RustTarget::from_triple("x86_64-unknown-linux-gnu");
    let macos_target = RustTarget::from_triple("aarch64-apple-darwin");
    let windows_target = RustTarget::from_triple("x86_64-pc-windows-msvc");
    assert!(linux_target.is_some() && macos_target.is_some() && windows_target.is_some());
    for (flags, target, active) in [
        (None, Some(None), &["by_feature"][..]),
        (None, None, UNIX_METHODS),
        (None, Some(linux_target), UNIX_METHODS),
        (None, Some(macos_target), UNIX_METHODS),
        (None, Some(windows_target), WINDOWS_METHODS),
        (Some(UNIX), None, UNIX_METHODS),
        (Some(WINDOWS), None, WINDOWS_METHODS),
        // Complete captured flags decide their manifest whatever the target.
        (Some(UNIX), Some(windows_target), UNIX_METHODS),
    ] {
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(10, 65_536),
        )
        .expect("input")
        .with_framework_context(&context, &[])
        .expect("context");
        let input = match flags {
            Some(flags) => input
                .with_rust_configuration(&manifest, flags)
                .expect("captured flags"),
            None => input,
        };
        let input = match target {
            Some(target) => input.with_rust_target(target),
            None => input,
        };
        let analysis = PackageAnalyzer::analyze(input, 1).expect("analysis");
        let publication = analysis.publication();
        for method in methods {
            assert!(
                publication
                    .objects
                    .iter()
                    .any(|object| object.name == method && object.id.is_some()),
                "{flags:?} {target:?}: {method} keeps its declaration and identity"
            );
            let from = format!("/Item/{method}");
            for (kind, from, to) in [
                (EdgeKind::Calls, from.as_str(), "/fixture/leaf"),
                (EdgeKind::Reads, from.as_str(), "/Item/value"),
                (EdgeKind::Contains, "/fixture/Item", from.as_str()),
            ] {
                let found = publication.edges.iter().any(|edge| {
                    edge.kinds.contains(&kind)
                        && edge.from.0.ends_with(from)
                        && edge.to.0.ends_with(to)
                });
                assert_eq!(
                    found,
                    active.contains(&method),
                    "{flags:?} {target:?}: {kind:?} {from} -> {to}"
                );
            }
        }
    }
}
