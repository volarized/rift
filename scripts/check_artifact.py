#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["mcp==1.26.0", "jsonschema==4.26.0", "psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Prove a supplied release binary serves reads, applies a patch, and stops."""

from __future__ import annotations

import argparse
import tempfile
from pathlib import Path

from rift_test_client import (
    Client,
    JsonObject,
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


def symbol_id(hit: JsonObject) -> str:
    """Use the read side's emitted identity as the edit address."""
    return string_value(
        object_value(hit.get("symbol"), "symbol").get("id"), "symbol.id"
    )


async def applied(client: Client, name: str, arguments: JsonObject) -> JsonObject:
    """Require an applied change; a validated refusal cannot pass an edit test."""
    result = await client.call(name, arguments)
    require(result.get("status") == "applied", f"{name} did not apply: {result}")
    return result


async def check_reads_and_patch(client: Client, root: Path) -> None:
    """Read known content, change it, and require both disk and MCP to reflect it."""
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
    patch = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn beacon_one() -> u8 { 1 }\n+pub fn beacon_one() -> u8 { 3 }\n"
    await applied(client, "patch", {"patch": patch})
    expected = SOURCE.replace("{ 1 }", "{ 3 }")
    require(
        (root / "lib.rs").read_bytes() == expected.encode("utf-8"),
        "patch wrote unexpected bytes",
    )
    after = await symbol_hit(client, "beacon_one")
    require(
        after.get("source") == "pub fn beacon_one() -> u8 { 3 }",
        f"patch was absent from reads: {after}",
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
                        await check_reads_and_patch(client, root)
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
    run_gate("artifact", check_artifact(binary, version), args.junit)
    print("artifact: reads, patch, and stop passed")


if __name__ == "__main__":
    main()
