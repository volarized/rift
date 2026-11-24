//! Static collection floors for the pinned Rust serialization corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires serde_json 1.0.154 in RIFT_CORPUS_SERDE_JSON"]
fn test_serde_json_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_SERDE_JSON").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    assert!(manifest.lines().any(|line| line == "version = \"1.0.154\""));
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
        assert!(file.syntax().left_out_symbols().is_empty());
        assert!(index.identity_coverage_complete(file.syntax().language())?);
        for symbol in file.syntax().symbols() {
            *declarations.entry(symbol.kind).or_default() += 1;
        }
    }
    assert_eq!(files, 37);

    for (kind, floor) in [
        ("function", 1092),
        ("impl_item", 231),
        ("type_alias", 204),
        ("constant", 115),
        ("field_declaration", 99),
        ("struct", 78),
        ("variant", 68),
        ("module", 43),
        ("macro", 26),
        ("trait", 17),
        ("enum", 13),
        ("static", 6),
        ("associated_type", 1),
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
        (EdgeKind::Calls, 508),
        (EdgeKind::Contains, 1030),
        (EdgeKind::Implements, 14),
        (EdgeKind::Imports, 102),
        (EdgeKind::HasType, 907),
        (EdgeKind::Reads, 526),
        (EdgeKind::Writes, 27),
        (EdgeKind::Overrides, 59),
        (EdgeKind::Aliases, 22),
    ] {
        let count = pairs.get(&kind).map_or(0, BTreeSet::len);
        assert!(count >= floor, "{kind:?}: {count} < {floor}");
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
    Ok(())
}
