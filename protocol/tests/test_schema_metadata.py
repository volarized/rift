"""Canonical JSON Schema metadata contracts."""

from rift.generate import schema_output


def test_read_tool_minimal_requests_are_published() -> None:
    tools = schema_output()["rift:entryPoints"]["mcp.tools"]

    assert tools["get_symbol"]["minimalRequest"] == {"name": "BaseModel"}
    assert tools["search"]["minimalRequest"] == {"query": "BaseModel"}
