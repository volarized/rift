//! A Python file that starts with spaces keeps its module-level names.

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};

/// Every edge of the workspace as kind, source and target, and the sites without a target.
fn rows(first: &str, package: &str) -> (Vec<String>, u64) {
    let directory = tempfile::tempdir().expect("workspace");
    let write = |path: &str, source: &str| {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, source).expect("source");
    };
    write(
        "pyproject.toml",
        "[project]\nname = 'pkg'\nversion = '1.0.0'\n",
    );
    write(
        "pkg/__init__.py",
        &format!("{package}from .a import leaf as leaf\n"),
    );
    write(
        "pkg/a.py",
        &format!(
            "{first}LIMIT = 1\n\n\ndef leaf():\n    pass\n\n\nclass Item:\n    def read(self):\n        pass\n"
        ),
    );
    write(
        "pkg/b.py",
        "from .a import Item, leaf\nfrom . import a\nfrom pkg import leaf as exported\n\n\nclass Sub(Item):\n    pass\n\n\ndef use(x: Item):\n    leaf()\n    x.read()\n    a.leaf()\n    exported()\n    return a.LIMIT\n",
    );
    let index = WorkspaceIndex::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let mut rows = index
        .edges()
        .edges()
        .map(|edge| {
            format!(
                "{} {} {}",
                edge.kind().as_ref(),
                edge.from().as_str(),
                edge.to().as_str()
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    (rows, index.static_targets_omitted().values().sum())
}

#[test]
fn python_names_do_not_depend_on_the_first_bytes_of_their_file() {
    let expected = rows("", "");
    for row in [
        "calls rift://symbol/local/python/pkg/b/use rift://symbol/local/python/pkg/a/leaf",
        "calls rift://symbol/local/python/pkg/b/use rift://symbol/local/python/pkg/a/Item/read",
        "extends rift://symbol/local/python/pkg/b/Sub rift://symbol/local/python/pkg/a/Item",
        "imports rift://symbol/local/python/pkg/b rift://symbol/local/python/pkg/a",
        "imports rift://symbol/local/python/pkg/b rift://symbol/local/python/pkg/a/leaf",
        "imports rift://symbol/local/python/pkg/b rift://symbol/local/python/pkg/a/Item",
        "reads rift://symbol/local/python/pkg/b/use rift://symbol/local/python/pkg/a/LIMIT",
        "aliases rift://symbol/local/python/pkg/leaf rift://symbol/local/python/pkg/a/leaf",
    ] {
        assert!(expected.0.iter().any(|stored| stored == row), "{row}");
    }
    for first in [
        "# c\n",
        "\"\"\"Doc.\"\"\"\n",
        "\n\n",
        "  \n",
        "\r\n",
        "\x0c\n",
        "\n# c\n",
        "\n\"\"\"Doc.\"\"\"\n",
        "\u{feff}",
    ] {
        assert_eq!(rows(first, ""), expected, "{first:?}");
        assert_eq!(rows("", first), expected, "package {first:?}");
        assert_eq!(rows(first, first), expected, "both {first:?}");
    }
}
