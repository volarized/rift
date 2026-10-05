"""Verify Rift crate dependency direction and single binary ownership."""

from __future__ import annotations

import collections
import difflib
import json
import pathlib
import re
from dataclasses import dataclass
from typing import Any

from rift_dev.commands import CargoCommand

# Cargo runs a test function only from a target it compiles, so a file holding one
# of these attributes is a suite rather than a helper another suite includes.
TEST_ATTRIBUTE = re.compile(r"#\[(?:tokio::)?test\b")

EXPECTED_EDGES = {
    "rift -> rift-core",
    "rift -> rift-error",
    "rift -> rift-index",
    "rift -> rift-mcp",
    "rift -> rift-protocol",
    "rift -> rift-provider",
    "rift -> rift-search",
    "rift -> rift-server",
    "rift -> rift-syntax",
    "rift -> rift-tracing",
    "rift-cloud-client -> rift-core",
    "rift-cloud-client -> rift-protocol",
    "rift-cloud-client -> rift-ranking",
    "rift-core -> rift-protocol",
    "rift-core -> rift-error",
    "rift-error -> rift-error-macros",
    "rift-error-codegen -> rift-error",
    "rift-dependency -> rift-core",
    "rift-dependency -> rift-protocol",
    "rift-history -> rift-core",
    "rift-history -> rift-error",
    "rift-history-store -> rift-core",
    "rift-history-store -> rift-error",
    "rift-history-store -> rift-protocol",
    "rift-history-store -> rift-ranking",
    "rift-analysis -> rift-core",
    "rift-analysis -> rift-error",
    "rift-cloud-client -> rift-analysis",
    "rift-analysis -> rift-protocol",
    "rift-analysis -> rift-provider",
    "rift-analysis -> rift-ranking",
    "rift-analysis -> rift-syntax",
    "rift-index -> rift-analysis",
    "rift-index -> rift-core",
    "rift-index -> rift-history",
    "rift-index -> rift-protocol",
    "rift-index -> rift-provider",
    "rift-index -> rift-ranking",
    "rift-index -> rift-syntax",
    "rift-index -> rift-error",
    "rift-index -> rift-tracing",
    "rift-lsp -> rift-core",
    "rift-lsp -> rift-error",
    "rift-lsp -> rift-provider",
    "rift-mcp -> rift-core",
    "rift-mcp -> rift-error",
    "rift-mcp -> rift-analysis",
    "rift-mcp -> rift-cloud-client",
    "rift-mcp -> rift-dependency",
    "rift-mcp -> rift-history",
    "rift-mcp -> rift-history-store",
    "rift-mcp -> rift-index",
    "rift-mcp -> rift-protocol",
    "rift-mcp -> rift-ranking",
    "rift-mcp -> rift-search",
    "rift-mcp -> rift-server",
    "rift-mcp -> rift-tracing",
    "rift-provider -> rift-core",
    "rift-provider -> rift-error",
    "rift-provider -> rift-protocol",
    "rift-ranking -> rift-core",
    "rift-ranking -> rift-error",
    "rift-search -> rift-core",
    "rift-search -> rift-index",
    "rift-search -> rift-ranking",
    "rift-search -> rift-error",
    "rift-schema-export -> rift-analysis",
    "rift-schema-export -> rift-cloud-client",
    "rift-schema-export -> rift-core",
    "rift-schema-export -> rift-mcp",
    "rift-schema-export -> rift-protocol",
    "rift-server -> rift-core",
    "rift-server -> rift-error",
    "rift-server -> rift-dependency",
    "rift-server -> rift-history",
    "rift-server -> rift-history-store",
    "rift-server -> rift-index",
    "rift-server -> rift-lsp",
    "rift-server -> rift-protocol",
    "rift-server -> rift-provider",
    "rift-server -> rift-ranking",
    "rift-server -> rift-search",
    "rift-server -> rift-syntax",
    "rift-syntax -> rift-core",
    "rift-syntax -> rift-error",
    "rift-syntax -> rift-protocol",
    "rift-syntax -> rift-provider",
    "rift-tracing -> rift-error",
}


def cargo_metadata() -> dict[str, Any]:
    """Load workspace package metadata from Cargo."""
    return json.loads(
        CargoCommand("metadata", "--no-deps", "--format-version", "1").output()
    )


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
            complaints.append(
                f"{package['name']}: {path} declares a test and has no [[test]] entry"
            )
        for path in testless:
            complaints.append(
                f"{package['name']}: {path} has a [[test]] entry and declares no test"
            )
    if complaints:
        raise RuntimeError("Rift test targets differ:\n" + "\n".join(complaints))


# rift-ranking states the retrieval decisions every index shares, and an
# external adapter must be able to build it with no storage and no model
# runtime. These are the crates that would make that false: the SQLite stack,
# the embedding runtime, and the HTTP client. A dependency reaching any of
# them means an algorithm moved back behind a storage boundary.
STORAGE_INDEPENDENT = "rift-ranking"
STORAGE_CRATES = frozenset(
    {
        "candle-core",
        "candle-nn",
        "candle-transformers",
        "libsqlite3-sys",
        "reqwest",
        "rig-core",
        "rusqlite",
        "toasty",
        "toasty-core",
        "toasty-driver-sqlite",
        "tokenizers",
    }
)


def resolved_closure(name: str) -> set[str]:
    """Return every crate the resolved dependency graph reaches from `name`."""
    tree = CargoCommand(
        "tree",
        "--package",
        name,
        "--edges",
        "normal",
        "--prefix",
        "none",
        "--no-dedupe",
        "--format",
        "{p}",
    ).output()
    return {
        line.split()[0]
        for line in tree.splitlines()
        if line.strip() and not line.startswith("[")
    }


def fail_storage_independence() -> None:
    """Refuse a storage or model-runtime crate in the shared ranking closure."""
    reached = sorted(resolved_closure(STORAGE_INDEPENDENT) & STORAGE_CRATES)
    if reached:
        raise RuntimeError(
            f"{STORAGE_INDEPENDENT} must build without storage or a model runtime, "
            f"and now reaches: {', '.join(reached)}"
        )


# The ownership check names who may import a tracing, logging, metrics, or
# resource-sampling backend. Only `rift-tracing` may; every other workspace
# package reaches those libraries through it. A library is identified by the
# package name Cargo resolves, so a renamed dependency (`rename` in the
# metadata) and a domain module that happens to be called `log` are told apart
# from the backend. It runs in report mode: it lists what it finds and never
# fails, because a later batch switches it to enforce once the consumers move.
TRACING_OWNER = "rift-tracing"
BACKEND_PACKAGES = frozenset(
    {
        "tracing",
        "log",
        "tracing-subscriber",
        "tracing-opentelemetry",
        "tracing-appender",
        "tracing-log",
        "env_logger",
        "metrics",
        "prometheus",
        "sysinfo",
        "tokio-metrics",
    }
)
BACKEND_PREFIXES = ("opentelemetry",)
FACADE_ALIAS = re.compile(r"\brift_tracing\b[^;{}]*?\bas\s+(tracing|log)\b")
CODE_FENCE = re.compile(r"^\s*//[/!]\s*```")


@dataclass(frozen=True, slots=True)
class OwnershipFinding:
    """One line outside `rift-tracing` that reaches a backend library."""

    package: str
    library: str
    path: str
    line: int
    text: str


def is_backend(package_name: str) -> bool:
    """Whether Cargo package `package_name` is a backend library the facade owns."""
    return package_name in BACKEND_PACKAGES or package_name.startswith(BACKEND_PREFIXES)


def backend_names(package: dict[str, Any]) -> dict[str, str]:
    """Map each local identifier a package imports a backend by to the library behind it.

    A dependency of any kind (normal, development, build, optional, target
    conditional) counts, and `rename` is the identifier the source uses, so
    `tracing_crate = { package = "tracing" }` is found under `tracing_crate`.
    """
    names: dict[str, str] = {}
    for dependency in package["dependencies"]:
        if is_backend(dependency["name"]):
            local = (dependency.get("rename") or dependency["name"]).replace("-", "_")
            names[local] = dependency["name"]
    return names


def rust_code_lines(text: str) -> list[tuple[int, str]]:
    """Return the lines that are code, with doctest bodies and without prose comments.

    A plain comment and doc prose never import anything. A doc comment's fenced
    block is a doctest that compiles, so its lines count.
    """
    lines: list[tuple[int, str]] = []
    fenced = False
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.strip()
        if stripped.startswith(("///", "//!")):
            if CODE_FENCE.match(line):
                fenced = not fenced
            elif fenced:
                lines.append((number, stripped[3:]))
        elif stripped.startswith("//"):
            continue
        else:
            fenced = False
            lines.append((number, line))
    return lines


def package_findings(package: dict[str, Any]) -> list[OwnershipFinding]:
    """List every backend import in the Rust files under one package's directory."""
    names = backend_names(package)
    patterns = [
        (
            library,
            re.compile(
                rf"(?<![\w:.]){re.escape(local)}\b(?=\s*::|\s*;|\s+as\b)"
                rf"|\bextern\s+crate\s+{re.escape(local)}\b"
            ),
        )
        for local, library in names.items()
    ]
    root = pathlib.Path(package["manifest_path"]).parent
    findings: list[OwnershipFinding] = []
    for path in sorted(root.rglob("*.rs")):
        if path.relative_to(root).parts[0] == "target":
            continue
        text = path.read_text(encoding="utf-8")
        for number, line in rust_code_lines(text):
            hits = {library for library, pattern in patterns if pattern.search(line)}
            if FACADE_ALIAS.search(line):
                hits.add("rift-tracing alias")
            findings.extend(
                OwnershipFinding(
                    package["name"], library, str(path), number, line.strip()
                )
                for library in sorted(hits)
            )
    return findings


def ownership_findings(packages: list[dict[str, Any]]) -> list[OwnershipFinding]:
    """List backend imports in every workspace package except `rift-tracing`."""
    return [
        finding
        for package in packages
        if package["name"] != TRACING_OWNER
        for finding in package_findings(package)
    ]


def ownership_report(
    packages: list[dict[str, Any]], findings: list[OwnershipFinding]
) -> list[str]:
    """Summarize ownership findings: one line per package, then the totals."""
    per_package: dict[str, collections.Counter[str]] = collections.defaultdict(
        collections.Counter
    )
    for finding in findings:
        per_package[finding.package][finding.library] += 1
    lines = [
        f"  {name}: {sum(counts.values())} ("
        + ", ".join(f"{library} {count}" for library, count in sorted(counts.items()))
        + ")"
        for name, counts in sorted(per_package.items())
    ]
    declared = sum(
        len(backend_names(package))
        for package in packages
        if package["name"] != TRACING_OWNER
    )
    lines.append(
        f"ownership (report only): {len(findings)} backend imports outside "
        f"{TRACING_OWNER} in {len(per_package)} packages, "
        f"{declared} backend dependencies declared there"
    )
    return lines


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
        "rift-schema-export:rift-schema-export",
        "rift:rift",
    ]
    if binaries != expected_binaries:
        raise RuntimeError(
            f"expected exactly {expected_binaries} binary targets, got {binaries}"
        )

    fail_test_targets(packages)
    fail_storage_independence()
    for line in ownership_report(packages, ownership_findings(packages)):
        print(line)
    return 0
