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
        "def mapping(values: t.Dict[Other, Item]) -> None:\n    for value in values:\n        value.read()\n",
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

#[test]
fn test_python_element_types_follow_container_position_and_view() {
    let header = "import collections.abc as cabc\nimport collections.abc\nfrom collections import abc, deque\n";
    for (signature, body) in [
        ("values: t.Dict[Item, Other]", "for value in values:"),
        ("values: dict[Other, Item]", "for value in values.values():"),
        ("values: dict[Item, Other]", "for value in values.keys():"),
        (
            "values: dict[Other, Item]",
            "for key, value in values.items():",
        ),
        (
            "values: t.Mapping[Item, Other]",
            "for value, key in values.items():",
        ),
        ("values: tuple[Item, ...]", "for value in values:"),
        (
            "values: list[tuple[Other, Item]]",
            "for key, value in values:",
        ),
        (
            "values: list[tuple[Item, ...]]",
            "for key, other, value in values:",
        ),
        ("values: list[list[Item]]", "for key, value in values:"),
        (
            "values: list[tuple[str, Item]]",
            "for [key, value] in values:",
        ),
        (
            "values: list[tuple[str, Item]]",
            "for (key, value) in values:",
        ),
        ("values: cabc.Sequence[Item]", "for value in values:"),
        (
            "values: collections.abc.Iterable[Item]",
            "for value in values:",
        ),
        (
            "values: abc.Mapping[str, Item]",
            "for key, value in values.items():",
        ),
        ("values: deque[Item]", "for value in values:"),
        (
            "values: collections.OrderedDict[str, Item]",
            "for value in values.values():",
        ),
        (
            "values: t.Generator[Item, None, Other]",
            "for value in values:",
        ),
        ("values: list[Item] | None", "for value in values:"),
        (
            "values: t.Optional[t.Dict[str, Item]]",
            "for value in values.values():",
        ),
        ("values: 'list[Item]'", "for value in values:"),
        (
            "groups: list[list[Item]]",
            "for values in groups:\n        for value in values:",
        ),
        (
            "groups: dict[str, list[Item]]",
            "for key, values in groups.items():\n        for value in values:",
        ),
        (
            "pair: tuple[Other, Item]",
            "key, value = pair\n    if value:",
        ),
        ("items: list[Item]", "key, value = items\n    if value:"),
    ] {
        let source = format!(
            "{header}def use({signature}) -> None:\n    {body}\n            value.read()\n"
        );
        assert_eq!(readers(&source), ["use"], "{signature} {body}");
    }
}

#[test]
fn test_python_element_types_follow_returns_names_and_comprehensions() {
    let observed = readers(
        "class Owner:\n    def all(self) -> list[Item]: return []\n    @property\n    def known(self) -> list[Item]: return []\n    def held(self):\n        for item in self.known:\n            item.read()\n    def named(self):\n        items = self.all()\n        for item in items:\n            item.read()\n    def kept(self, pair: tuple[Item, Other]):\n        self.item, self.other = pair\n    def later(self):\n        self.item.read()\ndef pairs() -> dict[str, Item]: return {}\ndef listed(values: list[Item]):\n    return [value.read() for value in values]\ndef generated(values: list[Item]):\n    return any(value.read() for value in values if value.read())\ndef entries():\n    return {key: value.read() for key, value in pairs().items()}\ndef unpacked():\n    item, count = split()\n    item.read()\ndef split() -> tuple[Item, int]: return Item(), 1\n",
    );
    assert_eq!(
        observed,
        [
            "Owner/held",
            "Owner/named",
            "Owner/later",
            "listed",
            "generated",
            "generated",
            "entries",
            "unpacked",
        ]
    );
}

#[test]
fn test_python_lists_built_by_appends_take_the_appended_class() {
    let observed = readers(
        "class Group:\n    def get(self, name) -> t.Optional[Item]: return None\n    def commands(self, names):\n        commands = []\n        for name in names:\n            cmd = self.get(name)\n            if cmd is None:\n                continue\n            commands.append((name, cmd))\n        if len(commands):\n            limit = max(len(cmd[0]) for cmd in commands)\n            for name, cmd in commands:\n                cmd.read()\ndef single(item: Item):\n    items = []\n    items.append(item)\n    items.append(Item())\n    items.sort()\n    for each in items:\n        each.read()\n",
    );
    assert_eq!(observed, ["Group/commands", "single"]);
}

#[test]
fn test_python_element_types_refuse_mismatched_and_computed_parts() {
    for body in [
        "def use(values: list[tuple[str, str, Item]]):\n    for key, value in values:\n        value.read()\n",
        "def use(values: list[tuple[Item, Other]]):\n    for key, value in values:\n        value.read()\n",
        "def use(values: list[tuple[Item, ...]]):\n    for key, *value in values:\n        value.read()\n",
        "def use(values: list[tuple[Item, Item]]):\n    for *key, value in values:\n        value.read()\n",
        "def use(values: list[tuple[int, tuple[Item, Item]]]):\n    for key, (value, other) in values:\n        value.read()\n",
        "def use(values: tuple[Item, Other]):\n    for value in values:\n        value.read()\n",
        "def use(values: dict[str, Item]):\n    for key, value in values:\n        value.read()\n",
        "def use(values: dict[str, Item]):\n    for key, value, other in values.items():\n        value.read()\n",
        "def use(values: list[Item]):\n    for value in values.values():\n        value.read()\n",
        "def use(values: list[Item] | set[Item]):\n    for value in values:\n        value.read()\n",
        "def use(values: list):\n    for value in values:\n        value.read()\n",
        "def use(values: list[tuple[str, Item]]):\n    for key, value in values:\n        value = Other()\n        value.read()\n",
        "def use(values: list[Item]):\n    for index, value in enumerate(values):\n        value.read()\n",
        "def use(value: tuple[Item, Other]):\n    value.read()\n",
        "import local.abc as cabc\nclass Mapping: pass\ndef use(values: cabc.Mapping[str, Item]):\n    for value in values.values():\n        value.read()\n",
        "class Store:\n    def items(self): return []\ndef use(values: Store):\n    for key, value in values.items():\n        value.read()\n",
        "def use(item: Item, other: Other):\n    values = []\n    values.append(item)\n    values.append(other)\n    for value in values:\n        value.read()\n",
        "def use(item: Item, more):\n    values = []\n    values.append(item)\n    values.extend(more)\n    for value in values:\n        value.read()\n",
        "def use(item: Item, more):\n    values = []\n    values.append(item)\n    values += more\n    for value in values:\n        value.read()\n",
        "def use(item: Item, flag):\n    values = []\n    values.append((item, flag))\n    for value, flag, other in values:\n        value.read()\n",
        "values = []\ndef use(item: Item):\n    values.append(item)\n    for value in values:\n        value.read()\n",
        "def use(item: Item):\n    values = []\n    for value in values:\n        value.read()\n",
        "def use(values):\n    return [value.read() for value in values]\n",
        "def use(value: list[Item]):\n    value.read()\n",
    ] {
        assert_eq!(readers(body), Vec::<String>::new(), "{body}");
    }
}

#[test]
fn test_python_cyclic_elements_and_attributes_stay_within_their_budget() {
    let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
    let mut source = format!("{HEADER}class Owner:\n    def __init__(self):\n");
    for name in names {
        source += &["        self.", name, " = Item()\n"].concat();
        for other in names {
            source += &["        self.", name, " = self.", other, "\n"].concat();
        }
    }
    source.push_str("    def use(self):\n");
    for name in names {
        source += &["        self.", name, ".read()\n"].concat();
    }
    source.push_str("def use(item: Item):\n    stack = []\n");
    for _ in names {
        source.push_str("    for each in stack: stack.append(each)\n");
    }
    source.push_str("    for each in stack: each.read()\n    item.read()\n");
    let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
    assert_eq!(result.unavailable_sources, 0);
    assert!(!result.exhausted);
    let owners = calls(&result, "main.py")
        .filter(|edge| target(edge).ends_with("/Item/read"))
        .filter_map(|edge| edge.edge_from.as_ref())
        .map(|from| from.as_str().to_owned())
        .collect::<Vec<_>>();
    let mut expected = vec!["rift://symbol/local/python/main/Owner/use"; names.len()];
    expected.push("rift://symbol/local/python/main/use");
    assert_eq!(owners, expected);
}

#[test]
fn test_python_appended_lists_refuse_uses_that_may_change_their_elements() {
    for uses in [
        "    values[:] = more\n",
        "    values[0] = more\n",
        "    del values[0]\n",
        "    fill(values)\n",
        "    fill(into=values)\n",
        "    add = values.append\n    add(more)\n",
        "    other = values\n    other.append(more)\n",
        "    values.insert(0, more)\n",
        "    values.extend(more)\n",
        "    values += more\n",
        "    def inner(): values.append(more)\n    inner()\n",
        "    keep = lambda: values\n",
        "    held = [values]\n",
        "    yield values\n",
        "    values = []\n",
        "    other = values = []\n",
    ] {
        let body = format!(
            "def use(item: Item, more):\n    values = []\n    values.append(item)\n{uses}    for value in values:\n        value.read()\n"
        );
        assert_eq!(readers(&body), Vec::<String>::new(), "{body}");
    }
}

#[test]
fn test_python_appended_lists_refuse_uses_written_before_the_list() {
    for body in [
        "def use(item: Item, more):\n    def add(): values.extend(more)\n    values = []\n    values.append(item)\n    add()\n    for value in values:\n        value.read()\n",
        "def use(item: Item, more):\n    add = lambda: values.extend(more)\n    values = []\n    values.append(item)\n    add()\n    for value in values:\n        value.read()\n",
        "def use(item: Item, flags):\n    for flag in flags:\n        if flag:\n            fill(values)\n        else:\n            values = []\n            values.append(item)\n        for value in values:\n            value.read()\n",
        "def use(item: Item):\n    other = values = []\n    other.append(Other())\n    values.append(item)\n    for value in values:\n        value.read()\n",
        "def use(item: Item):\n    values = other = []\n    other.append(Other())\n    values.append(item)\n    for value in values:\n        value.read()\n",
    ] {
        assert_eq!(readers(body), Vec::<String>::new(), "{body}");
    }
    // A list of another scope is judged by its own uses alone.
    assert_eq!(
        readers(
            "def first(more):\n    values = []\n    fill(values)\ndef use(item: Item):\n    values = []\n    values.append(item)\n    for value in values:\n        value.read()\ndef last():\n    values = []\n    return values\n"
        ),
        ["use"]
    );
}

#[test]
fn test_python_appended_lists_hold_the_values_appended_before_the_read() {
    for (body, expected) in [
        (
            "def use(item: Item):\n    values = []\n    for value in values:\n        value.read()\n    values.append(item)\n",
            Vec::<&str>::new(),
        ),
        (
            "def use(item: Item, other: Other):\n    values = []\n    values.append(item)\n    for value in values:\n        value.read()\n    values.append(other)\n",
            vec!["use"],
        ),
        (
            "def use(item: Item, flag):\n    values = []\n    while flag:\n        for value in values:\n            value.read()\n        values.append(item)\n",
            vec!["use"],
        ),
        (
            "def use(item: Item):\n    values = []\n    values.append(item)\n    count = len(values)\n    values.sort()\n    values.pop()\n    obj.values = 1\n    call(values=1)\n    for value in values:\n        value.read()\n",
            vec!["use"],
        ),
    ] {
        assert_eq!(readers(body), expected, "{body}");
    }
}

#[test]
fn test_python_redefined_properties_supply_no_elements() {
    for property in [
        "    @property\n    def items(self) -> list[Item]: return []\n    @property\n    def items(self) -> list[Other]: return []\n",
        "    @property\n    def items(self) -> list[Item]: return []\n    @items.getter\n    def items(self) -> list[Other]: return []\n",
        "    @property\n    def items(self) -> list[Item]: return []\n    def items(self) -> list[Other]: return []\n",
        "    @property\n    def items(self) -> list[Item]: return []\n    items: list[Other] = []\n",
        "    def property(call): return call\n    @property\n    def items(self) -> list[Item]: return []\n",
    ] {
        let body = format!(
            "class Owner:\n{property}    def loop(self):\n        for item in self.items:\n            item.read()\n    def unpack(self):\n        first, second = self.items\n        first.read()\n"
        );
        assert_eq!(readers(&body), Vec::<String>::new(), "{body}");
    }
    let body = "class Owner:\n    @property\n    def items(self) -> list[Item]: return []\n    @items.setter\n    def items(self, value): pass\n    def loop(self):\n        for item in self.items:\n            item.read()\n    def unpack(self):\n        first, second = self.items\n        first.read()\n";
    assert_eq!(readers(body), ["Owner/loop", "Owner/unpack"]);
}

#[test]
fn test_python_comprehension_iterables_read_the_enclosing_name() {
    assert_eq!(
        readers("def use(value: list[Item]):\n    return [value.read() for value in value]\n"),
        ["use"]
    );
    assert_eq!(
        readers(
            "def use(pairs: list[Item], groups: list[list[Other]]):\n    return [value.read() for value in pairs for pairs in groups]\n"
        ),
        ["use"]
    );
    for body in [
        "def use(pairs: list[Other], groups: list[list[Item]]):\n    return [value.read() for value in pairs for pairs in groups]\n",
        "def use(pairs: list[Other], groups: list[tuple[Item, Item]]):\n    return [value.read() for value in pairs for pairs, *rest in groups]\n",
        "def use(groups: list[list[Item]]):\n    return [group.read() for group in groups for group in group]\n",
        "def use(groups: list[Other], value: list[Item]):\n    return [value.read() for group in groups for value in value]\n",
    ] {
        assert_eq!(readers(body), Vec::<String>::new(), "{body}");
    }
}
