#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Verify Rift crate dependency direction and single binary ownership."""

from __future__ import annotations

import difflib
import json
import subprocess
from typing import Any

EXPECTED_EDGES = {
    "rift -> rift-core",
    "rift -> rift-index",
    "rift -> rift-mcp",
    "rift -> rift-model",
    "rift -> rift-protocol",
    "rift -> rift-provider",
    "rift -> rift-server",
    "rift -> rift-syntax",
    "rift-index -> rift-core",
    "rift-index -> rift-model",
    "rift-index -> rift-provider",
    "rift-index -> rift-syntax",
    "rift-mcp -> rift-core",
    "rift-mcp -> rift-index",
    "rift-mcp -> rift-protocol",
    "rift-mcp -> rift-server",
    "rift-model -> rift-core",
    "rift-provider -> rift-core",
    "rift-server -> rift-core",
    "rift-server -> rift-index",
    "rift-server -> rift-protocol",
    "rift-server -> rift-provider",
    "rift-server -> rift-syntax",
    "rift-syntax -> rift-core",
    "rift-syntax -> rift-provider",
}


def cargo_metadata() -> dict[str, Any]:
    """Load workspace package metadata from Cargo."""
    process = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        capture_output=True,
        check=False,
        text=True,
    )
    if process.returncode != 0:
        raise RuntimeError(process.stderr.strip() or "cargo metadata failed")
    return json.loads(process.stdout)


def rift_packages(metadata: dict[str, Any]) -> list[dict[str, Any]]:
    """Return workspace packages owned by Rift."""
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        raise TypeError("cargo metadata packages must be a list")
    return [package for package in packages if package["name"].startswith("rift")]


def dependency_edges(packages: list[dict[str, Any]]) -> set[str]:
    """Collect internal package dependency edges."""
    return {
        f"{package['name']} -> {dependency['name']}"
        for package in packages
        for dependency in package["dependencies"]
        if dependency["name"].startswith("rift")
    }


def fail_edges(actual: set[str]) -> None:
    """Report dependency drift as unified diff."""
    expected_lines = [f"{edge}\n" for edge in sorted(EXPECTED_EDGES)]
    actual_lines = [f"{edge}\n" for edge in sorted(actual)]
    difference = "".join(
        difflib.unified_diff(expected_lines, actual_lines, "expected", "actual")
    )
    raise RuntimeError(f"Rift dependency edges differ:\n{difference}")


def main() -> int:
    """Check exact internal edges and binary targets."""
    packages = rift_packages(cargo_metadata())
    edges = dependency_edges(packages)
    if edges != EXPECTED_EDGES:
        fail_edges(edges)

    binaries = sorted(
        f"{package['name']}:{target['name']}"
        for package in packages
        for target in package["targets"]
        if "bin" in target["kind"]
    )
    # rift is the only released binary; rift-schema-export is the repo-internal
    # generator that writes docs/protocol from the Rust protocol models.
    if binaries != ["rift-protocol:rift-schema-export", "rift:rift"]:
        raise RuntimeError(
            "expected exactly rift:rift and rift-protocol:rift-schema-export "
            f"binary targets, got {binaries}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
