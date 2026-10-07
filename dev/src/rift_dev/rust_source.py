"""Shared Rust source parsing helpers."""

from __future__ import annotations

import re

import tree_sitter_rust
from tree_sitter import Language, Parser

COMMENT_TYPES = {"line_comment", "block_comment"}
TEST_ATTRIBUTE = re.compile(
    rb"\bcfg\s*\(\s*(?:test\s*\)|all\([^)]*\btest\b)|"
    rb"^#\[(?:[\w:]+::)?test\s*\]$"
)


def code_rows(root, contents: bytes, entire_test: bool) -> tuple[set[int], set[int]]:
    """Partition token-bearing rows, retaining production when a row contains both."""
    production: set[int] = set()
    tests: set[int] = set()
    work = [(root, entire_test)]
    while work:
        node, inside_test = work.pop()
        if node.type in COMMENT_TYPES:
            continue
        if not node.children:
            last = node.end_point.row - (node.end_point.column == 0)
            destination = tests if inside_test else production
            destination.update(range(node.start_point.row + 1, last + 2))
            continue
        attributes = []
        for child in node.children:
            if child.type in COMMENT_TYPES:
                continue
            if child.type == "attribute_item":
                attributes.append(child)
                continue
            selected_test = inside_test or any(
                TEST_ATTRIBUTE.search(contents[item.start_byte : item.end_byte])
                for item in attributes
            )
            work.extend((item, selected_test) for item in attributes)
            attributes.clear()
            work.append((child, selected_test))
        work.extend((item, inside_test) for item in attributes)
    return production, tests - production


def test_rows(contents: bytes) -> set[int]:
    """Return one-based source rows compiled only for test configurations."""
    parser = Parser(Language(tree_sitter_rust.language()))
    tree = parser.parse(contents)
    return code_rows(tree.root_node, contents, False)[1]


def measurement_elapsed_rows(contents: bytes) -> set[int]:
    """Return elapsed calls on values returned by `measure_elapsed!`."""
    parser = Parser(Language(tree_sitter_rust.language()))
    tree = parser.parse(contents)

    def nodes(root):
        if root is None:
            return
        work = [root]
        while work:
            node = work.pop()
            yield node
            work.extend(reversed(node.children))

    def is_in_call_scope(declaration, call_blocks: set[int]) -> bool:
        parent = declaration.parent
        while parent is not None and parent.type != "block":
            parent = parent.parent
        return parent is not None and parent.start_byte in call_blocks

    def bound_to_measurement(identifier) -> bool:
        name = contents[identifier.start_byte : identifier.end_byte]
        function_scope = identifier.parent
        while function_scope is not None and function_scope.type != "function_item":
            function_scope = function_scope.parent
        if function_scope is None:
            return False
        call_blocks = set()
        block = identifier.parent
        while block is not None and block != function_scope:
            if block.type == "block":
                call_blocks.add(block.start_byte)
            block = block.parent
        declarations = [
            node
            for node in nodes(function_scope)
            if node.type == "let_declaration"
            and node.start_byte < identifier.start_byte
            and is_in_call_scope(node, call_blocks)
            and name in identifiers(node.child_by_field_name("pattern"))
        ]
        latest = max(declarations, key=lambda node: node.start_byte, default=None)
        return latest is not None and contains_measurement(
            latest.child_by_field_name("value")
        )

    def contains_measurement(node) -> bool:
        if node is None:
            return False
        if node.type == "macro_invocation":
            return bool(
                re.search(
                    rb"\bmeasure_elapsed\s*!",
                    contents[node.start_byte : node.end_byte],
                )
            )
        if node.type == "call_expression":
            function = node.child_by_field_name("function")
            if function is not None and function.type == "field_expression":
                field = function.child_by_field_name("field")
                receiver = function.child_by_field_name("value")
                if (
                    field is not None
                    and contents[field.start_byte : field.end_byte] == b"ok"
                    and receiver is not None
                    and receiver.type == "identifier"
                ):
                    return bound_to_measurement(receiver)
                return contains_measurement(receiver)
            return contains_measurement(function)
        if node.type in {"field_expression", "try_expression", "await_expression"}:
            return any(contains_measurement(child) for child in node.named_children)
        return False

    def identifiers(node) -> set[bytes]:
        return {
            contents[child.start_byte : child.end_byte]
            for child in nodes(node)
            if child.type == "identifier"
        }

    def is_measurement_map(call) -> bool:
        function = call.child_by_field_name("function")
        if function is None or function.type != "field_expression":
            return False
        field = function.child_by_field_name("field")
        return (
            field is not None
            and contents[field.start_byte : field.end_byte] == b"map"
            and contains_measurement(function.child_by_field_name("value"))
        )

    rows: set[int] = set()
    for call in nodes(tree.root_node):
        if call.type != "call_expression":
            continue
        function = call.child_by_field_name("function")
        if function is None or function.type != "field_expression":
            continue
        field = function.child_by_field_name("field")
        receiver = function.child_by_field_name("value")
        if (
            field is None
            or contents[field.start_byte : field.end_byte] != b"elapsed"
            or receiver is None
            or receiver.type != "identifier"
        ):
            continue
        name = contents[receiver.start_byte : receiver.end_byte]

        # A closure passed to `map` can call `elapsed` on the measurement tuple item.
        closure = call.parent
        while closure is not None and closure.type != "closure_expression":
            closure = closure.parent
        if closure is not None:
            parameters = closure.child_by_field_name("parameters")
            parent = closure.parent
            while parent is not None and parent.type != "call_expression":
                parent = parent.parent
            if (
                parameters is not None
                and name in identifiers(parameters)
                and parent is not None
                and is_measurement_map(parent)
            ):
                rows.add(call.start_point.row + 1)
                continue

        # A local binding can hold the value returned by `measure_elapsed!`.
        function_scope = call.parent
        while function_scope is not None and function_scope.type != "function_item":
            function_scope = function_scope.parent
        if function_scope is None:
            continue
        call_blocks = set()
        block = call.parent
        while block is not None and block != function_scope:
            if block.type == "block":
                call_blocks.add(block.start_byte)
            block = block.parent
        declarations = [
            node
            for node in nodes(function_scope)
            if node.type == "let_declaration"
            and node.start_byte < call.start_byte
            and is_in_call_scope(node, call_blocks)
            and name in identifiers(node.child_by_field_name("pattern"))
        ]
        latest = max(declarations, key=lambda node: node.start_byte, default=None)
        if latest is not None and contains_measurement(
            latest.child_by_field_name("value")
        ):
            rows.add(call.start_point.row + 1)
            continue

        # `if let` can bind the measurement from a local result returned by the macro.
        condition = call.parent
        while condition is not None and condition.type != "if_expression":
            condition = condition.parent
        if condition is None:
            continue
        binding = condition.child_by_field_name("condition")
        if binding is None or binding.type != "let_condition":
            continue
        value = binding.child_by_field_name("value")
        if (
            name in identifiers(binding.child_by_field_name("pattern"))
            and value is not None
            and value.type == "identifier"
            and contents[value.start_byte : value.end_byte] == name
            and latest is not None
            and contains_measurement(latest.child_by_field_name("value"))
        ):
            rows.add(call.start_point.row + 1)
    return rows


def stored_duration_elapsed_rows(contents: bytes) -> set[int]:
    """Return `elapsed` accessors on values bound by `StoreClose::Closed`."""
    parser = Parser(Language(tree_sitter_rust.language()))
    tree = parser.parse(contents)

    def nodes(root):
        if root is None:
            return
        work = [root]
        while work:
            node = work.pop()
            yield node
            work.extend(reversed(node.children))

    def identifiers(node) -> set[bytes]:
        return {
            contents[child.start_byte : child.end_byte]
            for child in nodes(node)
            if child.type == "identifier"
        }

    rows: set[int] = set()
    for arm in nodes(tree.root_node):
        if arm.type != "match_arm":
            continue
        pattern = arm.child_by_field_name("pattern")
        value = arm.child_by_field_name("value")
        if pattern is None or value is None or pattern.type != "match_pattern":
            continue
        if value.type != "macro_invocation":
            continue
        variants = [
            child for child in nodes(pattern) if child.type == "tuple_struct_pattern"
        ]
        bindings = set()
        for variant in variants:
            path = variant.child_by_field_name("type")
            if path is not None and contents[path.start_byte : path.end_byte].endswith(
                b"StoreClose::Closed"
            ):
                bindings.update(identifiers(variant))
        if not bindings:
            continue
        macro_source = contents[value.start_byte : value.end_byte]
        for name in bindings:
            for match in re.finditer(
                re.escape(name) + rb"\s*\.\s*elapsed\s*\(", macro_source
            ):
                row = (
                    value.start_point.row
                    + macro_source[: match.start()].count(b"\n")
                    + 1
                )
                rows.add(row)
    return rows
