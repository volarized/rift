"""Drive read tools and resources through a validating MCP client."""

from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

from rift_dev.check_artifact import (
    check_external_change,
    check_reads,
    incoming_references,
    lay_out_workspace,
    symbol_hit,
    symbol_id,
)
from rift_dev.rift_test_client import (
    Client,
    Server,
    array_value,
    gate_deadline,
    object_value,
    require,
    string_value,
    verify_version,
)

AGENT_SECONDS = 300.0
READ_TOOLS = {"search", "get_symbol", "nodes"}
RESOURCE_URIS = {"rift://map", "rift://workspace", "rift://logs"}


PYTHON_SOURCE = "def serve(port: int) -> int:\n    return port\n\n\ndef caller() -> int:\n    return serve(8080)\n"
PYTHON_CONFIGURATION = """
[languages.python.lsp]
embedded = "ty"
startup_timeout = "2m"
request_timeout = "2m"
retry = { attempts = 12, delay = "250ms", delay_limit = "2s" }
"""


async def check_embedded_references(client: Client, root: Path) -> None:
    """Require the embedded engine to resolve a caller through a read request."""
    seed = symbol_id(await symbol_hit(client, "serve", "python"))
    caller = symbol_id(await symbol_hit(client, "caller", "python"))
    answer = await incoming_references(client, seed)
    resolved = []
    for value in array_value(answer.get("results"), "search.results"):
        hit = object_value(value, "search hit")
        symbol = object_value(
            object_value(hit.get("hit"), "hit").get("symbol"), "symbol"
        )
        if symbol.get("id") == caller:
            resolved.append(hit)
    require(len(resolved) == 1, f"engine references omitted caller: {answer}")
    hops = array_value(resolved[0].get("traversal_path"), "traversal_path")
    require(len(hops) == 1, f"expected one reference step: {hops}")
    hop = object_value(hops[0], "hop")
    relationship = object_value(hop.get("relationship"), "relationship")
    require(
        relationship.get("from") == caller and relationship.get("to") == seed,
        f"reference endpoints differ: {relationship}",
    )
    require(
        relationship.get("derivation") == "resolution",
        f"reference was not resolved by the engine: {relationship}",
    )
    require(
        relationship.get("facets") == ["references"],
        f"reference facets differ: {relationship}",
    )
    require(hop.get("direction") == "incoming", f"reference direction differs: {hop}")
    require(
        (root / "service.py").read_bytes() == PYTHON_SOURCE.encode(),
        "reference read changed source bytes",
    )


async def check_resources(client: Client) -> None:
    """Read every advertised resource and assert its useful payload."""
    async with asyncio.timeout(client.call_seconds):
        listed = await client.session.list_resources()
    require(
        listed.nextCursor is None, "resource listing requires a new pagination case"
    )
    observed = {str(resource.uri) for resource in listed.resources}
    require(observed == RESOURCE_URIS, f"resource coverage differs: {observed}")
    workspace = await client.resource("rift://workspace")
    require(
        bool(array_value(workspace.get("languages"), "workspace.languages")),
        "workspace has no languages",
    )
    source = array_value(workspace.get("source"), "workspace.source")
    require(bool(source), "workspace has no source")
    object_value(workspace.get("pagination"), "workspace.pagination")
    string_value(
        workspace.get("configuration_revision"), "workspace.configuration_revision"
    )
    orientation = await client.resource("rift://map")
    require(
        bool(array_value(orientation.get("languages"), "map.languages")),
        "map has no languages",
    )
    string_value(orientation.get("revision"), "map.revision")
    logs = await client.resource("rift://logs")
    require(
        bool(array_value(logs.get("records"), "logs.records")),
        "startup recorded no logs",
    )


async def declaration_node(client: Client, name: str) -> str:
    """Prove nodes returns the declaration emitted by get_symbol."""
    hit = await symbol_hit(client, name)
    node = string_value(hit.get("node"), "hit.node")
    span = object_value(hit.get("range"), "hit.range")
    position = span.get("start")
    require(type(position) is int, f"range.start is not an integer: {span}")
    listing = await client.call("nodes", {"path": "lib.rs", "position": position})
    nodes = array_value(listing.get("nodes"), "nodes.nodes")
    require(
        any(object_value(entry, "node").get("id") == node for entry in nodes),
        f"nodes omitted the declaration address {node}: {listing}",
    )
    return node


async def check_agent(binary: Path, version: str | None = None) -> None:
    """Exercise all tools through a real proxy over one disposable workspace."""
    async with gate_deadline("agent", AGENT_SECONDS):
        if version is not None:
            verify_version(binary, version)
        with tempfile.TemporaryDirectory(prefix="rift-agent-") as directory:
            base = Path(directory)
            root = base / "workspace"
            root.mkdir()
            lay_out_workspace(root)
            configuration = root / "rift.toml"
            configuration.write_text(
                configuration.read_text() + PYTHON_CONFIGURATION,
                encoding="utf-8",
                newline="",
            )
            (root / "service.py").write_text(
                PYTHON_SOURCE, encoding="utf-8", newline=""
            )
            with Server(binary, root, base / "server.log") as server:
                try:
                    async with server.connect() as client:
                        await check_resources(client)
                        await check_reads(client)
                        await check_external_change(client, root)
                        await check_embedded_references(client, root)
                        await declaration_node(client, "beacon_one")
                        client.require_complete(READ_TOOLS)
                    server.stop()
                except BaseException as error:
                    error.add_note(server.read_log())
                    raise
