//! Files without tokens stay in the index.

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

/// File contents that declare a module alone.
const WITHOUT_TOKENS: [&str; 6] = ["", "\n", "   \n\n", "\r\n", "\t", "# c\n"];

fn build(files: &[(&str, &str)]) -> (tempfile::TempDir, WorkspaceIndex) {
    let directory = tempfile::tempdir().expect("workspace");
    for (path, source) in files {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, source).expect("source");
    }
    let index = WorkspaceIndex::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    (directory, index)
}

fn declares(index: &WorkspaceIndex, identity: &str) -> bool {
    index
        .normalized_graph()
        .records()
        .iter()
        .filter_map(|record| record.identity())
        .any(|declared| declared.as_str() == identity)
}

#[test]
fn python_modules_without_tokens_are_declared_and_contained() {
    for text in WITHOUT_TOKENS {
        let (_directory, index) = build(&[
            (
                "pyproject.toml",
                "[project]\nname = 'pkg'\nversion = '1.0.0'\n",
            ),
            ("pkg/__init__.py", "x = 1\n"),
            ("pkg/a.py", text),
        ]);
        assert!(
            index.warnings().is_empty(),
            "{text:?}: {:?}",
            index.warnings()
        );
        for identity in [
            "rift://symbol/local/python/pkg",
            "rift://symbol/local/python/pkg/x",
            "rift://symbol/local/python/pkg/a",
        ] {
            assert!(declares(&index, identity), "{text:?}: {identity}");
        }
        assert!(
            index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Contains
                    && edge.from().as_str() == "rift://symbol/local/python/pkg"
                    && edge.to().as_str() == "rift://symbol/local/python/pkg/x"
            }),
            "{text:?}"
        );
        let module = index
            .files()
            .find(|file| file.path().as_str() == "pkg/a.py")
            .expect("indexed file");
        assert_eq!(module.source(), text);
    }
}

#[test]
fn no_language_refuses_a_file_without_tokens() {
    for text in ["", "\n", "   \n\n", "\r\n", "\t"] {
        for path in [
            "a.rs", "a.py", "a.pyx", "a.ts", "a.tsx", "a.js", "a.mjs", "a.css", "a.html", "a.vue",
            "a.svelte", "a.md", "a.yaml", "a.c", "a.cpp", "a.jsonc",
        ] {
            let (_directory, index) = build(&[(path, text)]);
            assert!(
                index.warnings().is_empty(),
                "{path} {text:?}: {:?}",
                index.warnings()
            );
        }
    }
}
