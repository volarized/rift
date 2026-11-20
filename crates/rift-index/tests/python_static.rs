//! Python static facts retain their source and resolved identities.

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};

fn build(files: &[(&str, &str)]) -> (tempfile::TempDir, WorkspaceIndex) {
    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[project]\nname = 'probe'\nversion = '0.1.0'\n",
    )
    .expect("project metadata");
    for (path, source) in files {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("source directory");
        std::fs::write(path, source).expect("source");
    }
    let index = WorkspaceIndex::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("static index");
    (directory, index)
}

#[test]
fn test_python_property_returns_store_calls_and_property_reads() {
    let source = "class Item:\n    def read(self): pass\nclass Owner:\n    @property\n    def item(self) -> Item: return Item()\n    def use(self): return self.item.read()\ndef use(owner: Owner): return owner.item.read()\ndef invalid(): return Owner.item.read()\n";
    let (_directory, index) = build(&[("main.py", source)]);
    let calls = index
        .edges()
        .edges()
        .filter(|edge| {
            edge.kind().as_ref() == "calls"
                && edge.to().as_str() == "rift://symbol/local/python/main/Item/read"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2, "{:?}", index.edges());
    for edge in calls {
        assert!(
            [
                "rift://symbol/local/python/main/Owner/use",
                "rift://symbol/local/python/main/use"
            ]
            .contains(&edge.from().as_str())
        );
        let binding = edge.occurrence();
        assert_eq!(binding.unit().key().as_str(), "main.py");
        let start = usize::try_from(binding.range().start()).expect("source start");
        let end = usize::try_from(binding.range().end()).expect("source end");
        assert!(["self.item.read()", "owner.item.read()"].contains(&&source[start..end]));
    }
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind().as_ref() == "reads"
                && edge.to().as_str() == "rift://symbol/local/python/main/Owner/item")
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_python_wildcard_import_stores_the_module_and_explicit_alias() {
    let source = "from .leaf import *\ndef use(): return leaf()\n";
    let (_directory, index) = build(&[
        ("pkg/__init__.py", source),
        ("pkg/leaf.py", "value = 1\ndef leaf(): pass\n"),
        ("bridge.py", "from pkg.leaf import value as value\n"),
    ]);
    let imported = index
        .edges()
        .edges()
        .find(|edge| {
            edge.kind().as_ref() == "imports"
                && edge.from().as_str() == "rift://symbol/local/python/pkg"
                && edge.to().as_str() == "rift://symbol/local/python/pkg/leaf"
        })
        .expect("stored wildcard module import");
    let binding = imported.occurrence();
    assert_eq!(binding.unit().key().as_str(), "pkg/__init__.py");
    let start = usize::try_from(binding.range().start()).expect("source start");
    let end = usize::try_from(binding.range().end()).expect("source end");
    assert_eq!(&source[start..end], "*");
    for (kind, from) in [
        ("imports", "rift://symbol/local/python/bridge"),
        ("aliases", "rift://symbol/local/python/bridge/value"),
    ] {
        assert!(
            index.edges().edges().any(|edge| {
                edge.kind().as_ref() == kind
                    && edge.from().as_str() == from
                    && edge.to().as_str() == "rift://symbol/local/python/pkg/leaf/value"
            }),
            "{kind}: {:?}",
            index.edges()
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .all(|edge| edge.kind().as_ref() != "calls")
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}
