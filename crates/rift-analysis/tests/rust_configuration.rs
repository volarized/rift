//! Static package facts selected by a captured Rust build configuration.
#![cfg(feature = "collector")]

use rift_analysis::{ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource};
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
