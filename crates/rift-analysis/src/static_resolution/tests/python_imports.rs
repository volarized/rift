use super::{ReferenceRole, WORK_MAX, calls, resolution, source_of, target};

fn imports<'source>(
    result: &'source super::StaticResolution,
    path: &str,
    source: &'source str,
) -> Vec<(&'source str, &'source str)> {
    let unit = format!("rift://source/project/{path}");
    result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.role == ReferenceRole::Import && edge.binding.unit().to_string() == unit
        })
        .map(|edge| (source_of(edge, source), target(edge)))
        .collect()
}

#[test]
fn test_python_module_qualifiers_keep_their_tokens_and_terminal_targets() {
    let package = "from .child import Thing, helper\n\
        from . import child\n\
        from . import child as alias\n\
        value = child.Thing()\n\
        other = alias.Thing()\n\
        direct = Thing()\n\
        def by_value(child: Thing):\n    return child.run()\n";
    let path = "pkg/__init__.py";
    let result = resolution(
        "python",
        &[
            (path, package),
            (
                "pkg/child.py",
                "class Thing:\n    def run(self): pass\ndef helper(): pass\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    let module = "rift://symbol/local/python/pkg/child";
    let class = "rift://symbol/local/python/pkg/child/Thing";
    assert_eq!(
        imports(&result, path, package),
        [
            ("Thing", class),
            ("helper", "rift://symbol/local/python/pkg/child/helper"),
            ("child", module),
            ("alias", module),
            (".child", module),
        ],
    );
    let qualifiers = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.role == ReferenceRole::Read
                && edge.binding.unit().key().as_str() == path
                && target(edge) == module
        })
        .map(|edge| (source_of(edge, package), edge.binding.range().start()))
        .collect::<Vec<_>>();
    let site = |written| {
        u64::try_from(package.find(written).expect("written qualifier"))
            .expect("qualifier byte offset")
    };
    assert_eq!(
        qualifiers,
        [
            ("child", site("child.Thing")),
            ("alias", site("alias.Thing"))
        ],
    );
    assert_eq!(
        calls(&result, path)
            .map(|call| (source_of(call, package), target(call)))
            .collect::<Vec<_>>(),
        [
            ("child.Thing()", class),
            ("alias.Thing()", class),
            ("Thing()", class),
            (
                "child.run()",
                "rift://symbol/local/python/pkg/child/Thing/run"
            ),
        ],
    );
    assert!(!result.ended);
}

#[test]
fn test_python_package_self_imports_bind_their_submodules() {
    let package = "from . import leaf, other as other\n";
    let sibling = "from . import leaf\ndef use(): return leaf.target()\n";
    let result = resolution(
        "python",
        &[
            ("pkg/__init__.py", package),
            ("pkg/leaf.py", "def target(): pass\n"),
            ("pkg/other.py", ""),
            ("pkg/sibling.py", sibling),
            (
                "main.py",
                "from pkg import leaf\ndef run(): leaf.target()\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(
        imports(&result, "pkg/__init__.py", package),
        [
            ("leaf", "rift://symbol/local/python/pkg/leaf"),
            ("other", "rift://symbol/local/python/pkg/other"),
        ],
        "{result:?}"
    );
    assert_eq!(
        imports(&result, "pkg/sibling.py", sibling),
        [("leaf", "rift://symbol/local/python/pkg/leaf")],
        "{result:?}"
    );
    for path in ["pkg/sibling.py", "main.py"] {
        assert_eq!(
            calls(&result, path).map(target).collect::<Vec<_>>(),
            ["rift://symbol/local/python/pkg/leaf/target"],
            "{path}: {result:?}"
        );
    }
    assert!(!result.exhausted);
}

#[test]
fn test_python_package_self_imports_refuse_missing_and_rebound_submodules() {
    let missing = "from . import absent\n";
    let rebound = "from . import leaf\nleaf = object()\n";
    for (package, name) in [(missing, "absent"), (rebound, "leaf")] {
        let sibling = format!("from . import {name}\ndef use(): return {name}.target()\n");
        let result = resolution(
            "python",
            &[
                ("pkg/__init__.py", package),
                ("pkg/leaf.py", "def target(): pass\n"),
                ("pkg/sibling.py", &sibling),
            ],
            &[],
            WORK_MAX,
        );
        assert!(
            imports(&result, "pkg/sibling.py", &sibling)
                .iter()
                .all(|(_, target)| *target != "rift://symbol/local/python/pkg/absent"),
            "{package}\n{result:?}"
        );
        assert_eq!(
            calls(&result, "pkg/sibling.py").count(),
            0,
            "{package}\n{result:?}"
        );
    }
}
