//! Python receivers taken from the declared element type of an iterated container.

use super::{WORK_MAX, calls, resolution, source_of, target};

const HEADER: &str = "import typing\nimport typing as t\nfrom typing import List, Sequence\nclass Item:\n    def read(self) -> int: return 1\nclass Other:\n    def read(self) -> int: return 2\n";

fn readers(body: &str) -> Vec<String> {
    let source = format!("{HEADER}{body}");
    let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
    calls(&result, "main.py")
        .filter(|edge| {
            source_of(edge, &source).ends_with(".read()")
                && target(edge) == "rift://symbol/local/python/main/Item/read"
        })
        .filter_map(|edge| edge.edge_from.as_ref())
        .map(|from| {
            from.as_str()
                .trim_start_matches("rift://symbol/local/python/main/")
                .to_owned()
        })
        .collect()
}

#[test]
fn test_python_loop_elements_take_the_declared_element_class() {
    let observed = readers(
        "class Holder:\n    items: t.List[Item]\n    def __init__(self, loaders: t.Sequence[Item]) -> None:\n        self.loaders = loaders\n    def fields(self) -> None:\n        for item in self.items:\n            item.read()\n    def each(self) -> None:\n        for loader in self.loaders:\n            loader.read()\ndef produce() -> typing.Iterable[Item]:\n    return []\ndef returned() -> None:\n    for field in produce():\n        field.read()\ndef builtin(values: list[Item]) -> None:\n    for value in values:\n        value.read()\ndef imported(values: Sequence['Item']) -> None:\n    for value in values:\n        value.read()\ndef optional(values: t.Optional[List[Item]]) -> None:\n    for value in values:\n        value.read()\n    for value in values:\n        value.read()\n",
    );
    assert_eq!(
        observed,
        [
            "Holder/fields",
            "Holder/each",
            "returned",
            "builtin",
            "imported",
            "optional",
            "optional",
        ]
    );
}

#[test]
fn test_python_loop_elements_refuse_unknown_and_shadowed_containers() {
    for body in [
        "def mapping(values: t.Dict[Item, Other]) -> None:\n    for value in values:\n        value.read()\n",
        "def either(values: t.List[Item]) -> None:\n    for value in values or []:\n        value.read()\n",
        "def untyped(values) -> None:\n    for value in values:\n        value.read()\n",
        "def unpacked(values: t.List[Item]) -> None:\n    for value, other in values:\n        value.read()\n",
        "def union(values: t.List[t.Union[Item, Other]]) -> None:\n    for value in values:\n        value.read()\n",
        "list = dict\ndef shadowed(values: list[Item]) -> None:\n    for value in values:\n        value.read()\n",
        "def rebound(values: t.List[Item]) -> None:\n    for value in values:\n        value = Other()\n        value.read()\n",
        "class Box:\n    pass\ndef local_generic(values: Box[Item]) -> None:\n    for value in values:\n        value.read()\n",
    ] {
        assert_eq!(readers(body), Vec::<String>::new(), "{body}");
    }
}
