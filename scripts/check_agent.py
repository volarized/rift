#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["mcp==1.26.0", "jsonschema==4.26.0", "psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Drive every served tool through a validating MCP client and require every edit to apply."""

from __future__ import annotations

import argparse
import asyncio
import tempfile
from pathlib import Path

from check_artifact import (
    applied,
    check_reads_and_patch,
    lay_out_workspace,
    symbol_hit,
    symbol_id,
)
from rift_test_client import (
    Client,
    Server,
    array_value,
    candidate_binary,
    gate_deadline,
    object_value,
    require,
    run_gate,
    string_value,
    verify_version,
    workspace_version,
)

AGENT_SECONDS = 300.0
READ_TOOLS = {"search", "get_symbol", "nodes"}
RESOURCE_URIS = {"rift://map", "rift://workspace", "rift://logs"}
PYTHON_SOURCE = (
    "def serve(port: int) -> int:\n    return port\n\n\nvalue = serve(8080)\n"
)
PYTHON_CONFIGURATION = """
[languages.python.lsp]
embedded = "ty"
startup_timeout = "2m"
request_timeout = "2m"
retry = { attempts = 12, delay = "250ms", delay_limit = "2s" }
"""


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
    array_value(workspace.get("hooks"), "workspace.hooks")
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
    """Prove nodes returns the witnessed identity emitted by get_symbol."""
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


async def check_symbol_edits(client: Client, root: Path) -> None:
    """Insert, replace, and remove through identities obtained from real reads."""
    anchor = symbol_id(await symbol_hit(client, "beacon_two"))
    await applied(
        client,
        "insert_symbol",
        {
            "anchor": anchor,
            "position": "after",
            "body": "\npub fn beacon_three() -> u8 { 3 }\n",
        },
    )
    third = symbol_id(await symbol_hit(client, "beacon_three"))
    await applied(
        client,
        "replace_symbol",
        {
            "symbol": third,
            "body": "pub fn beacon_three() -> u8 { 4 }",
        },
    )
    require(
        (await symbol_hit(client, "beacon_three")).get("source")
        == "pub fn beacon_three() -> u8 { 4 }",
        "replace_symbol did not publish its body",
    )
    await check_node_edits(client, root)
    await applied(client, "remove_symbol", {"symbol": third, "force": True})
    removed = await client.call("get_symbol", {"name": "beacon_three"})
    require(
        removed.get("hits") == [],
        f"remove_symbol left its declaration indexed: {removed}",
    )
    require(
        "beacon_three" not in (root / "lib.rs").read_text(),
        "remove_symbol left source bytes",
    )


async def check_node_edits(client: Client, root: Path) -> None:
    """Apply witnessed node changes and prove an old witness refuses without writing."""
    third = await declaration_node(client, "beacon_three")
    await applied(
        client,
        "insert_node",
        {
            "anchor": third,
            "position": "after",
            "body": "\npub fn beacon_four() -> u8 { 5 }\n",
        },
    )
    fourth = await declaration_node(client, "beacon_four")
    await applied(
        client,
        "replace_node",
        {
            "node": fourth,
            "body": "pub fn beacon_four() -> u8 { 6 }",
        },
    )
    after = await symbol_hit(client, "beacon_four")
    require(
        after.get("source") == "pub fn beacon_four() -> u8 { 6 }",
        "replace_node did not publish",
    )
    before_refusal = (root / "lib.rs").read_bytes()
    stale = await client.call(
        "replace_node", {"node": fourth, "body": "pub fn beacon_four() {}"}
    )
    require(stale.get("status") == "refused", f"stale witness was accepted: {stale}")
    require(
        (root / "lib.rs").read_bytes() == before_refusal,
        "stale witness changed source bytes",
    )
    fresh = await declaration_node(client, "beacon_four")
    await applied(client, "remove_node", {"node": fresh, "force": True})
    removed = await client.call("get_symbol", {"name": "beacon_four"})
    require(
        removed.get("hits") == [],
        f"remove_node left its declaration indexed: {removed}",
    )


async def check_rename_and_move(client: Client, root: Path) -> None:
    """Prove the embedded engine renames both the declaration and its caller."""
    symbol = symbol_id(await symbol_hit(client, "serve", "python"))
    await applied(client, "rename_symbol", {"symbol": symbol, "new_name": "handle"})
    renamed = (root / "service.py").read_bytes()
    require(
        renamed == PYTHON_SOURCE.replace("serve", "handle").encode("utf-8"),
        f"rename left unexpected bytes: {renamed}",
    )
    await symbol_hit(client, "handle", "python")
    old = await client.call("get_symbol", {"name": "serve", "language": "python"})
    require(old.get("hits") == [], f"rename left its old declaration indexed: {old}")
    original = (root / "README.md").read_bytes()
    await applied(client, "move_file", {"from": "README.md", "to": "notes/README.md"})
    require(not (root / "README.md").exists(), "move_file left its old path")
    require(
        (root / "notes/README.md").read_bytes() == original, "move_file changed bytes"
    )
    moved = await client.call(
        "search",
        {
            "query": "Beacon",
            "target": "file",
            "paths": {"include": ["notes/README.md"]},
        },
    )
    require(
        bool(array_value(moved.get("results"), "search.results")),
        f"moved file was not indexed: {moved}",
    )


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
                        await check_reads_and_patch(client, root)
                        await check_symbol_edits(client, root)
                        await check_rename_and_move(client, root)
                        client.require_complete(READ_TOOLS)
                    server.stop()
                except BaseException:
                    print(server.read_log())
                    raise


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--version")
    parser.add_argument("--junit", type=Path)
    args = parser.parse_args()
    binary = candidate_binary(args.binary, args.target)
    version = args.version or workspace_version()
    run_gate("agent", check_agent(binary, version), args.junit)
    print("agent: every tool applied or read, every resource read, stop passed")


if __name__ == "__main__":
    main()
