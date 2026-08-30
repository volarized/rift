#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Verify Rift crate dependency direction and single binary ownership."""

from __future__ import annotations

import difflib
import json
import pathlib
import re
import subprocess
from typing import Any

# Cargo runs a test function only from a target it compiles, so a file holding one
# of these attributes is a suite rather than a helper another suite includes.
TEST_ATTRIBUTE = re.compile(r"#\[(?:tokio::)?test\b")

EXPECTED_EDGES = {
    "rift -> rift-core",
    "rift -> rift-index",
    "rift -> rift-mcp",
    "rift -> rift-protocol",
    "rift -> rift-provider",
    "rift -> rift-search",
    "rift -> rift-server",
    "rift -> rift-syntax",
    "rift-binding -> rift-core",
    "rift-binding -> rift-provider",
    "rift-core -> rift-protocol",
    "rift-history -> rift-core",
    "rift-index -> rift-core",
    "rift-index -> rift-history",
    "rift-index -> rift-protocol",
    "rift-index -> rift-provider",
    "rift-index -> rift-syntax",
    "rift-lsp -> rift-core",
    "rift-lsp -> rift-provider",
    "rift-mcp -> rift-core",
    "rift-mcp -> rift-history",
    "rift-mcp -> rift-index",
    "rift-mcp -> rift-protocol",
    "rift-mcp -> rift-search",
    "rift-mcp -> rift-server",
    "rift-provider -> rift-core",
    "rift-search -> rift-core",
    "rift-search -> rift-index",
    "rift-server -> rift-core",
    "rift-server -> rift-history",
    "rift-server -> rift-index",
    "rift-server -> rift-lsp",
    "rift-server -> rift-protocol",
    "rift-server -> rift-provider",
    "rift-server -> rift-search",
    "rift-server -> rift-syntax",
    "rift-syntax -> rift-binding",
    "rift-syntax -> rift-core",
    "rift-syntax -> rift-protocol",
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


def unlisted_test_suites(package: dict[str, Any]) -> tuple[list[str], list[str]]:
    """Return test files a package leaves unlisted, and listed files holding no test.

    A package that turns off Cargo's own test discovery lists every suite as a
    `[[test]]` target. A suite added to `tests/` without an entry compiles into
    nothing and stops running, so the two sets have to match exactly: every file
    under `tests/` that declares a test is a listed target, and every listed target
    declares one. A file declaring none is a helper another suite reaches with
    `mod <name>;`, and listing it would compile it as a suite of its own.
    """
    manifest = pathlib.Path(package["manifest_path"])
    if "autotests = false" not in manifest.read_text(encoding="utf-8"):
        return ([], [])
    listed = {
        pathlib.Path(target["src_path"]).resolve()
        for target in package["targets"]
        if "test" in target["kind"]
    }
    directory = manifest.parent / "tests"
    declares_a_test = {
        entry.resolve()
        for entry in sorted(directory.glob("*.rs"))
        if TEST_ATTRIBUTE.search(entry.read_text(encoding="utf-8"))
    }
    unlisted = sorted(str(path) for path in declares_a_test - listed)
    testless = sorted(str(path) for path in listed - declares_a_test)
    return (unlisted, testless)


def fail_test_targets(packages: list[dict[str, Any]]) -> None:
    """Report every suite left out of a manifest, and every listed file holding no test."""
    complaints: list[str] = []
    for package in packages:
        unlisted, testless = unlisted_test_suites(package)
        for path in unlisted:
            complaints.append(f"{package['name']}: {path} declares a test and has no [[test]] entry")
        for path in testless:
            complaints.append(f"{package['name']}: {path} has a [[test]] entry and declares no test")
    if complaints:
        raise RuntimeError("Rift test targets differ:\n" + "\n".join(complaints))


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
    # rift is the only released binary, and rift-schema-export is the
    # repo-internal generator that writes the served tool surface into
    # docs/public. Engine behavior is proven against real language servers, so
    # the workspace ships no test engine of its own.
    expected_binaries = [
        "rift-mcp:rift-schema-export",
        "rift:rift",
    ]
    if binaries != expected_binaries:
        raise RuntimeError(
            f"expected exactly {expected_binaries} binary targets, got {binaries}"
        )

    fail_test_targets(packages)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
