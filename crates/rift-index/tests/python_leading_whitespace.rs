//! A Python file that starts with spaces keeps its module-level names.

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};

/// Every edge of a workspace as kind, source and target, and the sites without a target.
fn rows(files: &[(&str, &str)]) -> (Vec<String>, u64) {
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
    for (path, source) in files {
        write(path, source);
    }
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

fn started(first: &str, package: &str) -> (Vec<String>, u64) {
    rows(&[
        (
            "pkg/__init__.py",
            &format!("{package}from .a import leaf as leaf\n"),
        ),
        (
            "pkg/a.py",
            &format!(
                "{first}LIMIT = 1\n\n\ndef leaf():\n    pass\n\n\nclass Item:\n    def read(self):\n        pass\n"
            ),
        ),
        (
            "pkg/b.py",
            "from .a import Item, leaf\nfrom . import a\nfrom pkg import leaf as exported\n\n\nclass Sub(Item):\n    pass\n\n\ndef use(x: Item):\n    leaf()\n    x.read()\n    a.leaf()\n    exported()\n    return a.LIMIT\n",
        ),
    ])
}

#[test]
fn python_names_do_not_depend_on_the_first_bytes_of_their_file() {
    let expected = started("", "");
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
        assert_eq!(started(first, ""), expected, "{first:?}");
        assert_eq!(started("", first), expected, "package {first:?}");
        assert_eq!(started(first, first), expected, "both {first:?}");
    }
}

#[test]
fn python_scopes_of_one_definition_files_stay_apart_from_the_module() {
    for (source, importer, stored) in [
        (
            "class Item:\n    def read(self): return Item()",
            "from .a import Item, read, self\ndef use(x: Item):\n    x.read()\n    read()\n",
            vec![
                "calls rift://symbol/local/python/pkg/a/Item/read rift://symbol/local/python/pkg/a/Item",
                "calls rift://symbol/local/python/pkg/b/use rift://symbol/local/python/pkg/a/Item/read",
                "imports rift://symbol/local/python/pkg/b rift://symbol/local/python/pkg/a/Item",
            ],
        ),
        (
            "def f(x):\n    y = 1\n    return f(x)",
            "from .a import f, x, y\ndef use():\n    f()\n    x()\n    y()\n",
            vec![
                "calls rift://symbol/local/python/pkg/a/f rift://symbol/local/python/pkg/a/f",
                "calls rift://symbol/local/python/pkg/b/use rift://symbol/local/python/pkg/a/f",
                "imports rift://symbol/local/python/pkg/b rift://symbol/local/python/pkg/a/f",
            ],
        ),
        (
            "lambda x: x()",
            "from .a import x\ndef use(): x()\n",
            Vec::new(),
        ),
        (
            "[x for x in names]",
            "from .a import x\ndef use(): x()\n",
            Vec::new(),
        ),
    ] {
        let expected = rows(&[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", &format!("# c\n{source}\n")),
            ("pkg/b.py", importer),
        ]);
        for row in stored {
            assert!(expected.0.iter().any(|stored| stored == row), "{row}");
        }
        for first in ["", "\n", "  \n", "\u{feff}"] {
            for last in ["", "\n"] {
                let observed = rows(&[
                    ("pkg/__init__.py", ""),
                    ("pkg/a.py", &format!("{first}{source}{last}")),
                    ("pkg/b.py", importer),
                ]);
                assert_eq!(observed, expected, "{first:?} {source} {last:?}");
            }
        }
    }
}

#[test]
fn python_member_reads_keep_their_class_when_its_span_matches_the_module() {
    let class = "class x:\n    from pkg.model import Other\n    y = Other()\n    y.leaf";
    let module = "from pkg.model import Other\ny = Other()\ny.leaf";
    let target = "rift://symbol/local/python/pkg/model/Other/leaf";
    let from = |owner| format!("reads rift://symbol/local/python/{owner} {target}");
    let owners = |source: &str| {
        let (rows, _) = rows(&[
            ("pkg/__init__.py", ""),
            (
                "pkg/model.py",
                "class Other:\n    def leaf(self): return 2\n",
            ),
            ("pkg/use.py", source),
        ]);
        rows.into_iter()
            .filter(|row| row.starts_with("reads ") && row.ends_with(target))
            .collect::<Vec<_>>()
    };
    for text in [
        class.to_owned(),
        format!("{class}\n"),
        format!("\n{class}"),
        format!("{class}\nafter = 0"),
        format!("\n{}", class.replace("    y.leaf", "    del x\n    y.leaf")),
    ] {
        assert_eq!(owners(&text), [from("pkg/use/x")], "{text:?}");
    }
    assert_eq!(owners(module), [from("pkg/use")]);
    assert_eq!(
        owners(&format!("{class}\n{module}")),
        [from("pkg/use"), from("pkg/use/x")],
    );
}
