//! Static collection floors for the pinned Zod corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires zod 4.6.7 in RIFT_CORPUS_ZOD"]
fn test_zod_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_ZOD").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("packages/zod/package.json"))?;
    assert!(manifest.contains("\"version\": \"4.6.7\""));
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec![
                "packages/zod/src/**/*.ts".into(),
                "packages/zod/package.json".into(),
            ],
            vec!["**/tests/**".into(), "**/benchmarks/**".into()],
            true,
        ),
        &TextFileInclusion::default(),
    )?;
    let mut declarations = BTreeMap::<_, usize>::new();
    let mut files = 0;
    for file in index
        .files()
        .filter(|file| file.syntax().language().name == "typescript")
    {
        files += 1;
        assert!(index.identity_coverage_complete(file.syntax().language())?);
        for symbol in file.syntax().symbols() {
            *declarations.entry(symbol.kind).or_default() += 1;
        }
    }
    assert_eq!(files, 125);
    for (kind, floor) in [
        ("property", 8030),
        ("variable", 3125),
        ("function", 832),
        ("method", 684),
        ("interface", 644),
        ("type_alias", 504),
        ("class", 53),
        ("enum_member", 36),
        ("namespace", 9),
        ("enum", 2),
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
        (EdgeKind::Calls, 2472),
        (EdgeKind::Contains, 16237),
        (EdgeKind::Extends, 669),
        (EdgeKind::HasType, 6345),
        (EdgeKind::Imports, 564),
        (EdgeKind::Reads, 7953),
        (EdgeKind::Writes, 285),
        (EdgeKind::Overrides, 606),
        (EdgeKind::Aliases, 3830),
        (EdgeKind::Implements, 1),
    ] {
        let count = pairs.get(&kind).map_or(0, BTreeSet::len);
        assert!(
            count >= floor,
            "{kind:?}: {count} < {floor} in {:?}",
            counts(&pairs)
        );
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
    Ok(())
}

fn counts<T>(pairs: &BTreeMap<EdgeKind, BTreeSet<T>>) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(kind, pairs)| (format!("{kind:?}"), pairs.len()))
        .collect()
}
