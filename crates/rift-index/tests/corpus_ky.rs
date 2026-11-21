//! Static collection floors for the pinned ky corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires ky 2.2.0 in RIFT_CORPUS_KY"]
fn test_ky_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_KY").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("package.json"))?;
    assert!(manifest.contains("\"version\": \"2.2.0\""));
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec!["source/**/*.ts".into(), "package.json".into()],
            vec![
                "media/**".into(),
                "node_modules/**".into(),
                "test/**".into(),
                "test-d/**".into(),
            ],
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
    assert_eq!(files, 31);
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
    ("variable", 409),
    ("property", 385),
    ("type_alias", 53),
    ("method", 51),
    ("function", 12),
    ("class", 10),
    ("interface", 2),
];
const FLOOR_EDGES: &[(EdgeKind, usize)] = &[
    (EdgeKind::Calls, 256),
    (EdgeKind::Contains, 897),
    (EdgeKind::Extends, 7),
    (EdgeKind::HasType, 194),
    (EdgeKind::Imports, 163),
    (EdgeKind::Overrides, 5),
    (EdgeKind::Reads, 765),
    (EdgeKind::Writes, 67),
];

fn counts<T>(pairs: &BTreeMap<EdgeKind, BTreeSet<T>>) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(kind, pairs)| (format!("{kind:?}"), pairs.len()))
        .collect()
}
