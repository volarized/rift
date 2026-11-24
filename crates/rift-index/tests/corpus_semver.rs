//! Static collection floors for the pinned Rust corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires semver 1.0.28 in RIFT_CORPUS_SEMVER"]
fn test_semver_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_SEMVER").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    assert!(manifest.lines().any(|line| line == "version = \"1.0.28\""));
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec!["src/**/*.rs".into(), "Cargo.toml".into()],
            vec![],
            true,
        ),
        &TextFileInclusion::default(),
    )?;
    let mut declarations = BTreeMap::<_, usize>::new();
    let mut files = 0;
    for file in index
        .files()
        .filter(|file| file.syntax().language().name == "rust")
    {
        files += 1;
        for symbol in file.syntax().symbols() {
            *declarations.entry(symbol.kind).or_default() += 1;
        }
    }
    assert_eq!(files, 8);
    for (kind, floor) in [
        ("function", 92),
        ("let_declaration", 5),
        ("impl_item", 51),
        ("struct", 11),
        ("enum", 3),
        ("variant", 25),
        ("field_declaration", 16),
        ("type_alias", 10),
        ("constant", 8),
        ("module", 7),
    ] {
        assert!(
            declarations.get(kind).copied().unwrap_or_default() >= floor,
            "{kind}: {declarations:?}"
        );
    }
    let mut pairs = BTreeMap::<_, BTreeSet<_>>::new();
    for edge in index.edges().edges() {
        pairs
            .entry(edge.kind())
            .or_default()
            .insert((edge.from(), edge.to()));
    }
    for (kind, floor) in [
        (EdgeKind::Calls, 140),
        (EdgeKind::Contains, 142),
        (EdgeKind::Imports, 27),
        (EdgeKind::HasType, 128),
        (EdgeKind::Reads, 125),
        (EdgeKind::Aliases, 1),
    ] {
        let count = pairs.get(&kind).map_or(0, BTreeSet::len);
        assert!(count >= floor, "{kind:?}: {count} < {floor}");
    }
    assert!(
        !index.edges().edges().any(|edge| edge
            .occurrence()
            .unit()
            .to_string()
            .ends_with("/src/serde.rs")),
        "the default-disabled module must not contribute stored edges"
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
    Ok(())
}
