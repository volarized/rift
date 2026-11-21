//! Static collection floors for the pinned Fastify corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires fastify 5.12.8 in RIFT_CORPUS_FASTIFY"]
fn test_fastify_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_FASTIFY").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("package.json"))?;
    assert!(manifest.contains("\"version\": \"5.12.8\""));
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec![
                "fastify.js".into(),
                "lib/**/*.js".into(),
                "package.json".into(),
            ],
            vec![
                "node_modules/**".into(),
                "test/**".into(),
                "types/**".into(),
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
    assert_eq!(files, 33);
    for (kind, floor) in FLOOR_DECLARATIONS {
        assert!(
            declarations.get(kind).copied().unwrap_or_default() >= *floor,
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
    for (kind, floor) in FLOOR_EDGES {
        let count = pairs.get(kind).map_or(0, BTreeSet::len);
        assert!(
            count >= *floor,
            "{kind:?}: {count} < {floor} in {:?}",
            counts(&pairs)
        );
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
    Ok(())
}

const FLOOR_DECLARATIONS: &[(&str, usize)] = &[
    ("property", 1279),
    ("variable", 1179),
    ("function", 233),
    ("method", 110),
    ("object", 15),
    ("class", 3),
];
const FLOOR_EDGES: &[(EdgeKind, usize)] = &[
    (EdgeKind::Calls, 260),
    (EdgeKind::Contains, 2453),
    (EdgeKind::Imports, 334),
    (EdgeKind::Reads, 2200),
    (EdgeKind::Writes, 218),
];

fn counts<T>(pairs: &BTreeMap<EdgeKind, BTreeSet<T>>) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(kind, pairs)| (format!("{kind:?}"), pairs.len()))
        .collect()
}
