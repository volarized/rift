use super::{WORK_MAX, calls, resolution, source_of, target};
use rift_protocol::read::EdgeKind;

#[test]
fn test_python_explicit_object_base_orders_like_an_implicit_base() {
    for (imports, object) in [
        ("", "object"),
        ("from builtins import object as root\n", "root"),
        ("import builtins as runtime\n", "runtime.object"),
    ] {
        let source = format!(
            "{imports}class A:\n    def m(self): pass\nclass B({object}): pass\nclass K(B, A): pass\ndef f(x: K): x.m()\n"
        );
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        assert_eq!(
            calls(&result, "main.py")
                .filter(|edge| source_of(edge, &source) == "x.m()")
                .map(target)
                .collect::<Vec<_>>(),
            ["rift://symbol/local/python/main/A/m"],
            "{source}"
        );
        assert!(!result.exhausted);
    }
}

#[test]
fn test_python_local_object_base_keeps_its_members() {
    let source =
        "class object:\n    def m(self): pass\nclass Child(object): pass\ndef f(x: Child): x.m()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/object/m"]
    );
    assert!(!result.exhausted);
}

#[test]
fn test_python_inconsistent_class_order_leaves_descendants_with_own_members() {
    for bases in ["A, B", "A, A", "object, A"] {
        let source = format!(
            "class A:\n    def m(self): pass\nclass B(A): pass\nclass X({bases}):\n    def x(self): pass\nclass C:\n    def c(self): pass\nclass Y(X, C):\n    def y(self): pass\nclass W(Y): pass\ndef f(y: Y, w: W):\n    y.x()\n    y.c()\n    w.x()\n    y.m()\n    w.y()\n    y.y()\n"
        );
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        assert_eq!(
            calls(&result, "main.py")
                .map(|edge| (source_of(edge, &source), target(edge)))
                .collect::<Vec<_>>(),
            [("y.y()", "rift://symbol/local/python/main/Y/y")],
            "{source}"
        );
        assert!(!result.exhausted);
    }
}

#[test]
fn test_python_generic_mro_keeps_the_shared_marker_order() {
    let source = "from typing import Generic, TypeVar\nT = TypeVar('T')\nclass Root:\n    def leaf(self): return 1\nclass Left(Generic[T], Root): pass\nclass Right(Generic[T]):\n    def leaf(self): return 2\nclass Child(Left[T], Right[T]):\n    def use(self): return self.leaf()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let leaf = calls(&result, "main.py")
        .find(|edge| source_of(edge, source) == "self.leaf()")
        .expect("inherited leaf call");
    assert_eq!(
        target(leaf),
        "rift://symbol/local/python/main/Right/leaf",
        "{result:?}"
    );
    assert_eq!(
        leaf.edge_from.as_ref().expect("caller").as_str(),
        "rift://symbol/local/python/main/Child/use"
    );
}

#[test]
fn test_python_generic_mro_refuses_external_members_and_inconsistent_order() {
    for (source, refused) in [
        (
            "from typing import Generic, TypeVar\nT = TypeVar('T')\nclass Root:\n    @classmethod\n    def __class_getitem__(cls, item): return cls\nclass Left(Generic[T], Root):\n    def use(self): return super().__class_getitem__(int)\n",
            "rift://symbol/local/python/main/Root/__class_getitem__",
        ),
        (
            "from typing import Generic, TypeVar\nT = TypeVar('T')\nclass Base(Generic[T]):\n    def leaf(self): pass\nclass Invalid(Generic[T], Base[T]):\n    def use(self): return self.leaf()\n",
            "rift://symbol/local/python/main/Base/leaf",
        ),
    ] {
        let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
        assert!(
            calls(&result, "main.py").all(|edge| target(edge) != refused),
            "{source}\n{result:?}"
        );
        assert!(!result.exhausted);
    }
}

#[test]
fn test_python_generic_ancestry_keeps_inherited_calls_and_overrides() {
    for import in [
        "from typing import Generic, TypeVar\n",
        "from typing_extensions import Generic, TypeVar\n",
    ] {
        let source = format!(
            "{import}T = TypeVar('T')\nclass Base(Generic[T]):\n    def leaf(self): return 1\n    def report(self): return {{}}\nclass Middle(Base[T], Generic[T]): pass\nclass Child(Middle[T], Generic[T]):\n    def report(self): return super().report()\n    def use(self): return self.leaf()\n"
        );
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        let observed = calls(&result, "main.py")
            .map(|edge| (source_of(edge, &source), target(edge)))
            .collect::<Vec<_>>();
        for pair in [
            (
                "super().report()",
                "rift://symbol/local/python/main/Base/report",
            ),
            ("self.leaf()", "rift://symbol/local/python/main/Base/leaf"),
        ] {
            assert!(observed.contains(&pair), "{result:?}");
        }
        assert!(
            result
                .occurrences
                .iter()
                .any(|edge| edge.edge_kind == Some(EdgeKind::Overrides)
                    && target(edge) == "rift://symbol/local/python/main/Base/report"
                    && edge.edge_from.as_ref().is_some_and(
                        |from| from.as_str() == "rift://symbol/local/python/main/Child/report"
                    )
                    && source_of(edge, &source) == "report"),
            "{result:?}"
        );
        assert!(!result.exhausted);
    }
}

#[test]
fn test_python_generic_ancestry_refuses_unknown_and_shadowed_bases() {
    for (import, middle, extra) in [
        ("import typing as t", "Base[T], Unknown", ""),
        (
            "import typing as t",
            "Base[T], t.Generic[T]",
            "t = unknown\n",
        ),
        ("import unrelated as t", "Base[T], t.Generic[T]", ""),
    ] {
        let source = format!(
            "{import}\n{extra}T = unknown\nclass Root:\n    def leaf(self): pass\nclass Base(Root, t.Generic[T]): pass\nclass Middle({middle}): pass\nclass Child(Middle[T], t.Generic[T]):\n    def use(self): return self.leaf()\n"
        );
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        assert!(
            calls(&result, "main.py")
                .all(|edge| target(edge) != "rift://symbol/local/python/main/Root/leaf"),
            "{source}\n{result:?}"
        );
    }
}
