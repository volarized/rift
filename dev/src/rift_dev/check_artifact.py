"""Prove a supplied binary serves reads and stops."""

from __future__ import annotations

import tempfile
from pathlib import Path

from rift_dev.rift_test_client import (
    Client,
    JsonObject,
    Server,
    array_value,
    gate_deadline,
    object_value,
    require,
    verify_version,
)

ARTIFACT_SECONDS = 240.0
CONFIGURATION = "[search.semantic]\ndisabled = true\n"
SOURCE = "pub fn beacon_one() -> u8 { 1 }\npub fn beacon_two() -> u8 { 2 }\n"


def lay_out_workspace(root: Path) -> None:
    """Use source-only files so language package resolution reaches no network."""
    (root / "rift.toml").write_text(CONFIGURATION, encoding="utf-8", newline="")
    (root / "lib.rs").write_text(SOURCE, encoding="utf-8", newline="")
    (root / "README.md").write_text("# Beacon\n", encoding="utf-8", newline="")


async def symbol_hit(client: Client, name: str, language: str = "rust") -> JsonObject:
    """Select an exact named declaration from the validated lookup answer."""
    answer = await client.call("get_symbol", {"name": name, "language": language})
    hits = array_value(answer.get("hits"), "get_symbol.hits")
    matches = [
        object_value(hit, "hit")
        for hit in hits
        if object_value(object_value(hit, "hit").get("symbol"), "symbol").get("name")
        == name
    ]
    require(len(matches) == 1, f"expected one {name} declaration: {answer}")
    return matches[0]


async def check_reads(client: Client) -> None:
    """Read known content and require MCP to return its source."""
    search = await client.call("search", {"query": "beacon_one", "target": "symbol"})
    require(
        bool(array_value(search.get("results"), "search.results")),
        f"search missed beacon_one: {search}",
    )
    before = await symbol_hit(client, "beacon_one")
    require(
        before.get("source") == "pub fn beacon_one() -> u8 { 1 }",
        f"unexpected source: {before}",
    )


async def check_artifact(binary: Path, version: str) -> None:
    """Run the real executable without compiling or replacing its bytes."""
    async with gate_deadline("artifact", ARTIFACT_SECONDS):
        verify_version(binary, version)
        with tempfile.TemporaryDirectory(prefix="rift-artifact-") as directory:
            base = Path(directory)
            root = base / "workspace"
            root.mkdir()
            lay_out_workspace(root)
            with Server(binary, root, base / "server.log") as server:
                try:
                    async with server.connect() as client:
                        await check_reads(client)
                    server.stop()
                except BaseException as error:
                    error.add_note(server.read_log())
                    raise
