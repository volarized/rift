//! Static collection floors for the pinned Express corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires express 5.1.0 in RIFT_CORPUS_EXPRESS"]
fn test_express_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_EXPRESS").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("package.json"))?;
    assert!(manifest.contains("\"version\": \"5.1.0\""));
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec![
                "index.js".into(),
                "lib/**/*.js".into(),
                "package.json".into(),
            ],
            vec![
                "benchmarks/**".into(),
                "examples/**".into(),
                "test/**".into(),
            ],
            true,
        ),
        &TextFileInclusion::default(),
    )?;
    let mut declarations = BTreeMap::<_, usize>::new();
    let mut files = 0;
    for file in index
        .files()
        .filter(|file| file.syntax().language().name == "javascript")
    {
        files += 1;
        assert!(index.identity_coverage_complete(file.syntax().language())?);
        for symbol in file.syntax().symbols() {
            *declarations.entry(symbol.kind).or_default() += 1;
        }
    }
    assert_eq!(files, 7);
    for (kind, floor) in [
        ("variable", 226),
        ("method", 46),
        ("property", 44),
        ("function", 24),
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
        (EdgeKind::Calls, 16),
        (EdgeKind::Contains, 325),
        (EdgeKind::Imports, 11),
        (EdgeKind::Reads, 319),
        (EdgeKind::Writes, 105),
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
