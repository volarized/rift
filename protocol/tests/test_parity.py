"""Executable parity contracts between the generated artifacts.

These automate the checks that caught real generator bugs during review:
proto fields silently dropped when a model field lacked a number, named
enums duplicated per message instead of referenced, and definitions left
unreachable from the documented entry points.
"""

import json
import re
from pathlib import Path
from unittest import TestCase

PROTOCOL = Path(__file__).parents[1]
PROTO_FILES = (
    "rift/core.proto",
    "rift/mcp.proto",
    "rift/adapter.proto",
    "rift/scip.proto",
)
FIELD = re.compile(
    r"^\s*(?:repeated\s+|optional\s+)?"
    r"(?:map\s*<[^>]+>|[\w.]+)\s+(\w+)\s*=\s*(\d+);"
)
ENUM_VALUE = re.compile(r"^\s*(\w+)\s*=\s*(\d+);")
OPENER = re.compile(r"^(message|enum|oneof|service)\s+(\w+)\s*\{")


def snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def parse_proto() -> (
    tuple[dict[str, dict[str, int]], dict[str, list[tuple[str, int]]]]
):
    """Return message fields and enum values, keyed by declaration path."""

    messages: dict[str, dict[str, int]] = {}
    enums: dict[str, list[tuple[str, int]]] = {}
    for rel in PROTO_FILES:
        stack: list[tuple[str, str]] = []
        for line in (PROTOCOL / rel).read_text().splitlines():
            stripped = line.strip()
            if stripped.startswith("//"):
                continue
            opened = OPENER.match(stripped)
            if opened:
                kind, name = opened.groups()
                if kind in ("message", "enum"):
                    named = [n for k, n in stack if k in ("message", "enum")]
                    path = ".".join([rel, *named, name])
                    (messages if kind == "message" else enums).setdefault(
                        path, {} if kind == "message" else []
                    )
                stack.append((kind, name))
                continue
            if stripped.endswith("{"):
                stack.append(("block", ""))
                continue
            if stripped == "}" or stripped == "};":
                if stack:
                    stack.pop()
                continue
            container = next(
                (
                    (k, n)
                    for k, n in reversed(stack)
                    if k in ("message", "enum")
                ),
                None,
            )
            if container is None:
                continue
            named = [n for k, n in stack if k in ("message", "enum")]
            path = ".".join([rel, *named])
            if container[0] == "message":
                field = FIELD.match(line)
                if field:
                    messages[path][field.group(1)] = int(field.group(2))
            else:
                value = ENUM_VALUE.match(line)
                if value:
                    enums[path].append((value.group(1), int(value.group(2))))
    return messages, enums


class ProtoParityTests(TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.document = json.loads((PROTOCOL / "mcp.json").read_text())
        cls.messages, cls.enums = parse_proto()
        cls.by_leaf: dict[str, list[str]] = {}
        for path in cls.messages:
            cls.by_leaf.setdefault(path.rsplit(".", 1)[-1], []).append(path)

    def test_every_numbered_json_field_reaches_the_proto(self) -> None:
        checked = 0
        missing: list[str] = []
        for name, definition in self.document["$defs"].items():
            properties = definition.get("properties")
            if not properties or name not in self.by_leaf:
                continue
            candidates = [self.messages[p] for p in self.by_leaf[name]]
            for prop, schema in properties.items():
                number = schema.get("rift:proto", {}).get("number")
                if number is None:
                    continue
                checked += 1
                names = {prop, snake(prop)}
                if not any(
                    fields.get(n) == number
                    for fields in candidates
                    for n in names
                ):
                    missing.append(f"{name}.{prop} = {number}")
        self.assertGreater(checked, 300, "parity check matched too few fields")
        self.assertEqual(missing, [])

    def test_field_numbers_are_unique_per_message(self) -> None:
        for path, fields in self.messages.items():
            numbers = list(fields.values())
            self.assertEqual(
                len(numbers), len(set(numbers)), f"duplicate number in {path}"
            )

    def test_every_enum_opens_with_a_prefixed_unspecified_zero(self) -> None:
        for path, values in self.enums.items():
            self.assertTrue(values, f"{path} declares no values")
            name, number = values[0]
            self.assertEqual(number, 0, f"{path} first value is {number}")
            self.assertTrue(
                name.endswith("_UNSPECIFIED"),
                f"{path} zero value is {name}",
            )

    def test_no_enum_content_is_declared_twice(self) -> None:
        by_content: dict[frozenset[tuple[str, int]], list[str]] = {}
        for path, values in self.enums.items():
            by_content.setdefault(frozenset(values), []).append(path)
        duplicated = {
            tuple(paths)
            for paths in by_content.values()
            if len(paths) > 1
        }
        self.assertEqual(duplicated, set())


class ReachabilityTests(TestCase):
    REF = re.compile(r'"\$ref":\s*"#/\$defs/([^"]+)"')

    def refs_of(self, node: object) -> set[str]:
        found = set(self.REF.findall(json.dumps(node)))

        def walk(value: object) -> None:
            if isinstance(value, dict):
                for key, child in value.items():
                    if key == "rift:contentTypes" and isinstance(child, dict):
                        found.update(
                            v for v in child.values() if isinstance(v, str)
                        )
                    elif key == "rift:arguments":
                        if isinstance(child, str):
                            found.add(child)
                        elif isinstance(child, dict):
                            found.update(
                                v for v in child.values() if isinstance(v, str)
                            )
                    else:
                        walk(child)
            elif isinstance(value, list):
                for item in value:
                    walk(item)

        walk(node)
        return found

    def test_every_definition_is_reachable_from_the_entry_points(self) -> None:
        document = json.loads((PROTOCOL / "mcp.json").read_text())
        definitions = document["$defs"]
        seen: set[str] = set()
        frontier = {
            ref
            for key, value in document.items()
            if key != "$defs"
            for ref in self.refs_of(value)
            if ref in definitions
        }
        while frontier:
            current = frontier.pop()
            if current in seen:
                continue
            seen.add(current)
            frontier |= {
                ref
                for ref in self.refs_of(definitions[current])
                if ref in definitions
            } - seen
        self.assertEqual(sorted(set(definitions) - seen), [])
