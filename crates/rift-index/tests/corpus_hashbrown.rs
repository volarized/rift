//! Static collection floors for the pinned Rust hash table corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires hashbrown 0.17.1 in RIFT_CORPUS_HASHBROWN"]
fn test_hashbrown_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_HASHBROWN").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    assert!(manifest.lines().any(|line| line == "version = \"0.17.1\""));
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
    assert_eq!(files, 28);

    for (kind, floor) in [
        ("function", 1006),
        ("impl_item", 439),
        ("field_declaration", 192),
        ("struct", 119),
        ("type_alias", 117),
        ("module", 44),
        ("constant", 35),
        ("variant", 16),
        ("enum", 8),
        ("trait", 5),
        ("macro", 4),
        ("let_declaration", 3),
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
        (EdgeKind::Calls, 852),
        (EdgeKind::Contains, 929),
        (EdgeKind::Implements, 2),
        (EdgeKind::Imports, 73),
        (EdgeKind::HasType, 560),
        (EdgeKind::Reads, 1046),
        (EdgeKind::Writes, 27),
        (EdgeKind::Overrides, 1),
        (EdgeKind::Aliases, 9),
    ] {
        let count = pairs.get(&kind).map_or(0, BTreeSet::len);
        assert!(count >= floor, "{kind:?}: {count} < {floor}");
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
    Ok(())
}
