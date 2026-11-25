//! Publication of imports from an accepted Cargo library root.

use super::*;
use rift_protocol::read::{EdgeDerivation, EdgeProvider};

const ROOT: &str = "rift://symbol/cargo/crates.io/beacon/rust/beacon";
const DEBUG: &str = "rift://symbol/stdlib/rust/rust/std/fmt/Debug";
const VEC_NEW: &str = "rift://symbol/stdlib/rust/rust/alloc/vec/Vec/new";
const MANIFEST: &str = "[package]\nname='beacon'\nversion='1.0.0'\nedition='2024'\n";
const SOURCE: &str = "// é\nuse std::fmt::Debug;\nuse untracked::Thing;\n\
    pub mod sibling;\nuse crate::sibling::Local;\n\
    pub use crate::sibling::AliasSource as PublicType;\n\
    pub fn run() -> Vec<u8> { Vec::new() }\n";
const SIBLING: &str = "use std::fmt::Write;\npub struct Local;\npub struct AliasSource;\n";

fn analyze_rust_root(files: Vec<(&str, &str)>, manifest: &str) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::Rust,
        files,
        None,
        Some(("Cargo.toml", manifest)),
    )
    .expect("captured Rust package")
}

#[test]
fn library_root_publishes_stored_external_import_and_preserves_authored_rows() {
    let (publication, files, semantics, _) = analyze_rust_root(
        vec![("src/lib.rs", SOURCE), ("src/sibling.rs", SIBLING)],
        MANIFEST,
    )
    .into_parts();
    let module = CoreSymbolId::new(ROOT.to_owned()).expect("library identity");
    let source = files
        .iter()
        .find(|source| source.file().path().as_str() == "src/lib.rs")
        .expect("library root");
    let unit = source.placement().unit();
    assert_eq!(source.placement().logical_identity("."), Some(&module));
    assert_eq!(semantics.edges().edge_module(unit), Some(&module));
    let mut objects = publication
        .objects
        .iter()
        .filter(|object| object.id.as_ref().is_some_and(|id| id.as_str() == ROOT));
    let object = objects.next().expect("published library root");
    assert!(objects.next().is_none());
    assert_eq!(object.kinds, [SymbolKind::Module]);
    assert_eq!(object.origin.source_kind, SourceKind::Synthetic);
    assert!(object.origin.location.is_none());
    assert!(
        publication
            .declarations
            .iter()
            .all(|declaration| declaration.symbol.as_str() != ROOT)
    );
    let root_imports = publication
        .edges
        .iter()
        .filter(|edge| edge.from.as_str() == ROOT && edge.kinds == [EdgeKind::Imports])
        .collect::<Vec<_>>();
    assert_eq!(root_imports.len(), 1);
    let external = root_imports[0];
    assert_eq!(external.to.as_str(), DEBUG);
    assert_eq!(external.provider, EdgeProvider::Static);
    assert_eq!(external.derivation, EdgeDerivation::Syntax);
    let occurrence = external.occurrence.as_ref().expect("written import span");
    assert_eq!(occurrence.unit.0, unit.to_string());
    assert_eq!(occurrence.range, TextRange { start: 10, end: 25 });
    let captured = semantics.edges().outgoing(&module);
    assert!(captured.iter().any(|edge| &edge.to_protocol() == external));
    assert!(captured.iter().any(|edge| {
        edge.kind() == EdgeKind::Imports && edge.to().as_str() == format!("{ROOT}/sibling/Local")
    }));
    assert_eq!(
        semantics.static_targets_omitted().get(&EdgeKind::Imports),
        Some(&1)
    );
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == format!("{ROOT}/run")
            && edge.to.as_str() == VEC_NEW
            && edge.kinds == [EdgeKind::Calls]
    }));
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == format!("{ROOT}/PublicType")
            && edge.to.as_str() == format!("{ROOT}/sibling/AliasSource")
            && edge.kinds == [EdgeKind::Aliases]
    }));
    assert!(publication.objects.iter().any(|object| {
        object
            .id
            .as_ref()
            .is_some_and(|id| id.as_str() == format!("{ROOT}/sibling"))
            && object.origin.source_kind == SourceKind::Authored
    }));
    let sibling = files
        .iter()
        .find(|source| source.file().path().as_str() == "src/sibling.rs")
        .expect("declared module");
    assert!(sibling.placement().logical_identity(".").is_none());
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == format!("{ROOT}/sibling")
            && edge.to.as_str() == "rift://symbol/stdlib/rust/rust/std/fmt/Write"
            && edge.kinds == [EdgeKind::Imports]
            && edge.occurrence.as_ref().is_some_and(|span| {
                span.unit.0 == sibling.placement().unit().to_string()
                    && span.range == TextRange { start: 4, end: 19 }
            })
    }));
    assert!(
        publication
            .edges
            .iter()
            .all(|edge| !edge.to.as_str().contains("untracked"))
    );
    publication.validate_facts().expect("covered publication");
}

#[test]
fn library_root_does_not_materialize_from_unchecked_scope_or_non_external_import() {
    let cycle = "mod lib;\nuse std::fmt::Debug;\npub fn run() {}";
    let malformed = "use std::fmt::Debug;\npub fn run() {}\npub fn broken( {";
    let missing = format!("{MANIFEST}\n[lib]\npath='src/absent.rs'\n");
    let binary = format!("{MANIFEST}\nautolib=false\n");
    let local = "pub mod sibling;\nuse crate::sibling::Local;\npub fn run() {}";
    let unresolved = "use untracked::Thing;\npub fn run() {}";
    for (path, source, manifest, anchored) in [
        ("src/lib.rs", cycle, MANIFEST, false),
        ("src/lib.rs", malformed, MANIFEST, false),
        ("src/lib.rs", SOURCE, missing.as_str(), false),
        ("src/main.rs", SOURCE, binary.as_str(), false),
        ("src/lib.rs", local, MANIFEST, true),
        ("src/lib.rs", unresolved, MANIFEST, true),
    ] {
        let (publication, files, _, _) =
            analyze_rust_root(vec![(path, source), ("src/sibling.rs", SIBLING)], manifest)
                .into_parts();
        let root = files
            .iter()
            .find(|file| file.file().path().as_str() == path)
            .expect("candidate source");
        assert_eq!(
            root.placement().logical_identity(".").is_some(),
            anchored,
            "{path}: {source}"
        );
        assert!(
            publication
                .objects
                .iter()
                .all(|object| object.id.as_ref().is_none_or(|id| id.as_str() != ROOT))
        );
        assert!(
            publication
                .edges
                .iter()
                .all(|edge| edge.from.as_str() != ROOT)
        );
    }
}

#[test]
fn package_publishes_proven_macro_calls_at_authored_invocations() {
    let source = r"
mod exported;
macro_rules! local { () => { 1u8 }; }
pub fn ordinary() -> u8 { 2 }
pub fn all() -> u8 { ordinary() + crate::exported!() + local!() }
";
    let other = "#[macro_export]\nmacro_rules! exported { () => { 3u8 }; }\n";
    let publication = analyze_rust_root(
        vec![("src/lib.rs", source), ("src/exported.rs", other)],
        MANIFEST,
    );
    let publication = publication.publication();
    let caller = format!("{ROOT}/all");
    let calls = publication
        .edges
        .iter()
        .filter(|edge| edge.from.as_str() == caller && edge.kinds == [EdgeKind::Calls])
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 3);
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "src/lib.rs")
        .expect("source unit");
    for (authored, target) in [
        ("ordinary()", "ordinary"),
        ("crate::exported!()", "exported/exported"),
        ("local!()", "local"),
    ] {
        let start = source.rfind(authored).expect("invocation site");
        let range = TextRange {
            start: u64::try_from(start).expect("invocation byte start"),
            end: u64::try_from(start + authored.len()).expect("invocation byte end"),
        };
        assert!(calls.iter().any(|edge| {
            edge.to.as_str() == format!("{ROOT}/{target}")
                && edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.unit == unit.unit && occurrence.range == range
                })
        }));
    }
}
