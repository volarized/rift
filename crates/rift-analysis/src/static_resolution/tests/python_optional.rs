//! Python receivers annotated `Optional[X]`, `Union[X, None]` or `X | None`.

use super::{WORK_MAX, calls, resolution, source_of, target};

const HEADER: &str = "import typing\nimport typing as t\nfrom typing import Optional, Union\nclass Item:\n    def read(self) -> int: return 1\nclass Other:\n    def read(self) -> int: return 2\ndef make() -> Item: return Item()\n";

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
fn test_python_optional_annotations_declare_their_receiver() {
    let observed = readers(
        "def typing_alias(obj: t.Optional[Item]) -> int:\n    if obj is None:\n        return 0\n    return obj.read()\ndef imported_name(obj: Optional[Item]) -> int:\n    return obj.read() if obj is not None else 0\ndef module_name(obj: typing.Optional['Item']) -> int:\n    return obj.read()\ndef union_none(obj: Union[Item, None]) -> int:\n    assert obj is not None\n    return obj.read()\ndef none_first(obj: t.Union[None, Item]) -> int:\n    return obj.read()\ndef pipe(obj: Item | None) -> int:\n    return obj.read()\ndef local(values) -> int:\n    ctx: t.Optional[Item] = values.get('item')\n    return ctx.read()\ndef rebound(obj: t.Optional[Item] = None) -> int:\n    obj = make()\n    return obj.read()\n",
    );
    assert_eq!(
        observed,
        [
            "typing_alias",
            "imported_name",
            "module_name",
            "union_none",
            "none_first",
            "pipe",
            "local",
            "rebound",
        ]
    );
}

#[test]
fn test_python_optional_annotations_refuse_unions_and_shadowed_names() {
    for body in [
        "def two(obj: Union[Item, Other]) -> int:\n    return obj.read()\n",
        "def two_none(obj: t.Union[Item, Other, None]) -> int:\n    return obj.read()\n",
        "def pipes(obj: Item | Other | None) -> int:\n    return obj.read()\n",
        "def nested(obj: t.Optional[t.Union[Item, Other]]) -> int:\n    return obj.read()\n",
        "def container(obj: t.List[Item]) -> int:\n    return obj.read()\n",
        "Optional = make\ndef shadowed(obj: Optional[Item]) -> int:\n    return obj.read()\n",
        "class Box:\n    pass\ndef local_generic(obj: Box[Item]) -> int:\n    return obj.read()\n",
    ] {
        assert_eq!(readers(body), Vec::<String>::new(), "{body}");
    }
}
