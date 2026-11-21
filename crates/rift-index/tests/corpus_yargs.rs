//! Static collection floors for the pinned yargs corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires yargs 18.2.0 in RIFT_CORPUS_YARGS"]
fn test_yargs_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_YARGS").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("package.json"))?;
    assert!(manifest.contains("\"version\": \"18.2.0\""));
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec![
                "index.mjs".into(),
                "browser.mjs".into(),
                "helpers/helpers.mjs".into(),
                "lib/**/*.ts".into(),
                "lib/**/*.mjs".into(),
                "lib/**/*.js".into(),
                "package.json".into(),
            ],
            vec![
                "helpers/package.json".into(),
                "node_modules/**".into(),
                "test/**".into(),
            ],
            true,
        ),
        &TextFileInclusion::default(),
    )?;
    let mut declarations = BTreeMap::<_, usize>::new();
    let mut files = 0;
    for file in index.files().filter(|file| {
        matches!(
            file.syntax().language().name.as_str(),
            "typescript" | "javascript"
        )
    }) {
        files += 1;
        // Identity coverage of TypeScript is incomplete while a parameter named `global`
        // fails to parse (four methods); the floors below pin what is collected around it.
        for symbol in file.syntax().symbols() {
            *declarations.entry(symbol.kind).or_default() += 1;
        }
    }
    assert_eq!(files, 26);
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
    ("property", 564),
    ("variable", 411),
    ("method", 287),
    ("function", 55),
    ("interface", 48),
    ("type_alias", 30),
    ("class", 5),
    ("object", 3),
];
const FLOOR_EDGES: &[(EdgeKind, usize)] = &[
    (EdgeKind::Aliases, 1),
    (EdgeKind::Calls, 488),
    (EdgeKind::Contains, 1326),
    (EdgeKind::Extends, 3),
    (EdgeKind::HasType, 577),
    (EdgeKind::Implements, 1),
    (EdgeKind::Imports, 116),
    (EdgeKind::Overrides, 14),
    (EdgeKind::Reads, 1574),
    (EdgeKind::Writes, 242),
];

fn counts<T>(pairs: &BTreeMap<EdgeKind, BTreeSet<T>>) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(kind, pairs)| (format!("{kind:?}"), pairs.len()))
        .collect()
}
