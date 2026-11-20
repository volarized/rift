//! Static collection floors for the pinned Hono corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
#[ignore = "requires hono 4.13.16 in RIFT_CORPUS_HONO"]
fn test_hono_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_HONO").ok_or("missing corpus")?);
    let manifest = std::fs::read_to_string(root.join("package.json"))?;
    assert!(manifest.contains("\"version\": \"4.13.16\""));
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec![
                "src/**/*.ts".into(),
                "src/**/*.tsx".into(),
                "package.json".into(),
            ],
            vec![
                "**/*.test.ts".into(),
                "**/*.test.tsx".into(),
                "benchmarks/**".into(),
                "build/**".into(),
                "docs/**".into(),
                "perf-measures/**".into(),
                "runtime-tests/**".into(),
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
        // One local variable is initialized from an expression the grammar cannot read
        // (`global` used as a value), so identity coverage of the language stays incomplete.
        for symbol in file.syntax().symbols() {
            *declarations.entry(symbol.kind).or_default() += 1;
        }
    }
    assert_eq!(files, 190);
    for (kind, floor) in [
        ("property", 2435),
        ("variable", 2350),
        ("type_alias", 385),
        ("method", 214),
        ("interface", 189),
        ("function", 55),
        ("class", 48),
        ("enum_member", 24),
        ("namespace", 8),
        ("object", 4),
        ("enum", 3),
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
        (EdgeKind::Calls, 796),
        (EdgeKind::Contains, 5534),
        (EdgeKind::Extends, 73),
        (EdgeKind::HasType, 2083),
        (EdgeKind::Imports, 945),
        (EdgeKind::Reads, 3628),
        (EdgeKind::Writes, 396),
        (EdgeKind::Overrides, 31),
        (EdgeKind::Aliases, 214),
        (EdgeKind::Implements, 7),
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
