use super::{WORK_MAX, calls, resolution, source_of, target};

#[test]
fn test_python_overloads_select_the_implementation_and_its_return_receiver() {
    let source = "from typing import overload\nclass Item:\n    def leaf(self): pass\n@overload\ndef build(value: int) -> Item: ...\n@overload\ndef build(value: str) -> Item: ...\ndef build(value) -> Item: return Item()\ndef use():\n    item = build(1)\n    item.leaf()\n    build('a').leaf()\ndef unrelated():\n    import os\n";
    let imported = "from main import build as create\ndef use(): return create(1).leaf()\n";
    let result = resolution(
        "python",
        &[("main.py", source), ("use.py", imported)],
        &[],
        WORK_MAX,
    );
    let observed = calls(&result, "main.py")
        .map(|edge| (source_of(edge, source), target(edge)))
        .collect::<Vec<_>>();
    for call in ["build(1)", "build('a')"] {
        assert!(
            observed.contains(&(call, "rift://symbol/local/python/main/build")),
            "{result:?}"
        );
    }
    for call in ["item.leaf()", "build('a').leaf()"] {
        assert!(
            observed.contains(&(call, "rift://symbol/local/python/main/Item/leaf")),
            "{result:?}"
        );
    }
    let implementation = calls(&result, "main.py")
        .find(|edge| source_of(edge, source) == "build(1)")
        .expect("implementation call");
    let range = implementation.target_binding.range();
    let start = usize::try_from(range.start()).expect("implementation start");
    let end = usize::try_from(range.end()).expect("implementation end");
    assert_eq!(
        &source[start..end],
        "def build(value) -> Item: return Item()"
    );
    assert_eq!(calls(&result, "use.py").count(), 2, "{result:?}");
    assert!(!result.exhausted);
}

#[test]
fn test_python_overloads_refuse_replaced_ambiguous_and_shadowed_bindings() {
    for source in [
        "from typing import overload\n@overload\ndef leaf(value: int): ...\ndef leaf(value): pass\ndef leaf(value): pass\ndef use(): leaf(1)\n",
        "from typing import overload\n@overload\ndef leaf(value: int): ...\ndef leaf(value): pass\nleaf = other\ndef use(): leaf(1)\n",
        "from typing import overload\noverload = other\n@overload\ndef leaf(value: int): ...\ndef leaf(value): pass\ndef use(): leaf(1)\n",
        "from typing import overload\nif enabled:\n    from unrelated import overload\n@overload\ndef leaf(value: int): ...\ndef leaf(value): pass\ndef use(): leaf(1)\n",
        "from typing import overload\n@overload\ndef leaf(value: int): ...\ndef leaf(value): pass\ndef use(leaf): leaf(1)\n",
        "from typing import overload\n@overload\ndef leaf(value: int): ...\nif enabled:\n    def leaf(value): pass\ndef use(): leaf(1)\n",
        "from typing import overload\ndef leaf(value): pass\n@overload\ndef leaf(value: int): ...\ndef use(): leaf(1)\n",
    ] {
        let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
        assert!(
            calls(&result, "main.py")
                .all(|edge| target(edge) != "rift://symbol/local/python/main/leaf"),
            "{source}\n{result:?}"
        );
    }
}

#[test]
fn test_python_overloads_keep_the_named_generic_return_owner() {
    let source = "from typing import Generic, TypeVar, overload\nT = TypeVar('T')\nclass Item(Generic[T]):\n    def leaf(self): pass\n@overload\ndef build(value: int) -> Item[int]: ...\n@overload\ndef build(value: str) -> Item[str]: ...\ndef build(value) -> Item[object]: pass\ndef use(): return build(1).leaf()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert!(
        calls(&result, "main.py")
            .any(|edge| target(edge) == "rift://symbol/local/python/main/Item/leaf"),
        "{result:?}"
    );
}

#[test]
fn test_python_overloads_resolve_methods_without_choosing_between_return_types() {
    let source = "import typing as t\nclass Item:\n    def leaf(self): pass\nclass Other:\n    def leaf(self): pass\nclass Owner:\n    @t.overload\n    def build(self, value: int) -> Item: ...\n    @t.overload\n    def build(self, value: str) -> Other: ...\n    def build(self, value): pass\ndef use(owner: Owner):\n    owner.build(1)\n    owner.build(1).leaf()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/Owner/build"; 2],
        "{result:?}"
    );
}

#[test]
fn test_python_overloads_refuse_a_local_typing_module() {
    let source = "from typing import overload\n@overload\ndef leaf(value: int): ...\ndef leaf(value): pass\ndef use(): leaf(1)\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("typing.py", "def overload(value): return value\n"),
        ],
        &[],
        WORK_MAX,
    );
    assert!(
        calls(&result, "main.py")
            .all(|edge| target(edge) != "rift://symbol/local/python/main/leaf"),
        "{result:?}"
    );
}
