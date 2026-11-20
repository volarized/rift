//! Static collection floors for the pinned Rust lazy initialization corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires once_cell 1.21.4 in RIFT_CORPUS_ONCE_CELL"]
fn test_once_cell_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_ONCE_CELL").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    assert!(manifest.lines().any(|line| line == "version = \"1.21.4\""));
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
    assert_eq!(files, 5);

    for (kind, floor) in [
        ("function", 131),
        ("impl_item", 61),
        ("field_declaration", 24),
        ("struct", 14),
        ("constant", 10),
        ("module", 9),
        ("enum", 6),
        ("static", 5),
        ("type_alias", 2),
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
        (EdgeKind::Calls, 107),
        (EdgeKind::Contains, 152),
        (EdgeKind::Imports, 4),
        (EdgeKind::HasType, 67),
        (EdgeKind::Reads, 96),
        (EdgeKind::Writes, 3),
        (EdgeKind::Aliases, 1),
    ] {
        let count = pairs.get(&kind).map_or(0, BTreeSet::len);
        assert!(count >= floor, "{kind:?}: {count} < {floor}");
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
    Ok(())
}
