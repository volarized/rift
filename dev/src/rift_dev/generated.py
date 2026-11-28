"""Export the docs and check the generated files distributed with the checkout."""

from __future__ import annotations

import difflib
import json
import tempfile
from pathlib import Path

from rift_dev.commands import REPOSITORY, CargoCommand, Command, fail

OPENAPI = "docs/public/global-api.openapi.json"
CLIENT = REPOSITORY / "crates/rift-cloud-client/src/generated.rs"

# Longest wait for the docs export, including Cargo builds.
BUILD_SECONDS_MAX = 1800.0


def schema_export(*arguments: str) -> CargoCommand:
    """`rift-schema-export`, built and run through Cargo, with `arguments`."""
    return CargoCommand("run", "--locked", "-q", "-p", "rift-schema-export", "--", *arguments)


def generate(check: bool) -> None:
    """Exports docs and writes or checks files shipped with the checkout."""
    CargoCommand("xtask", "docs").with_timeout(BUILD_SECONDS_MAX).run()
    if check:
        schema_export("--check", "--plugin").run()
        schema_export("--check", "--global-contract", "docs").run()
        check_client()
    else:
        schema_export("--plugin").run()
        CargoCommand("xtask", "analyzer-manifest").run()
        write_client(CLIENT)


def write_client(destination: Path) -> None:
    """Generates the global API client's Rust types into `destination`."""
    document = json.loads((REPOSITORY / OPENAPI).read_text(encoding="utf-8"))
    request = document["components"]["schemas"]["EdgeSearchRequest"]
    if request.get("type") != "object" or "properties" not in request:
        fail("edge request must retain its shared object schema")
    # oas3-gen 0.28 treats this object-presence condition as enum<Value>, then
    # emits a struct constructor for it. Rust owns the fields and admission;
    # the published contract retains its complete condition unchanged.
    request.pop("anyOf", None)
    with tempfile.TemporaryDirectory() as directory:
        source_contract = Path(directory) / "global-api.openapi.json"
        source_contract.write_text(json.dumps(document), encoding="utf-8")
        Command(
            "oas3-gen",
            "generate",
            "types",
            "-q",
            "--enum-mode",
            "relaxed",
            "--no-ordered-collections",
            "-i",
            source_contract,
            "-o",
            destination,
        ).run()

    # oas3-gen leaves these u64 bounds unsuffixed. validator also records each as an
    # error parameter, where Rust otherwise infers i32 and refuses the literal.
    source = destination.read_text(encoding="utf-8")
    destination.write_text(
        source.replace(f"//! Source: {source_contract}", f"//! Source: {OPENAPI}")
        .replace(
            "#[validate(range(min = 0, max = 9007199254740991))]",
            "#[validate(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]",
        )
        .replace(
            "#[validate(range(min = 0, max = 2147483647))]",
            "#[validate(range(min = 0_u64, max = 2_147_483_647_u64))]",
        ),
        encoding="utf-8",
    )


def check_client() -> None:
    """Fails with a unified diff when the committed client differs from a fresh one."""
    with tempfile.TemporaryDirectory() as directory:
        fresh = Path(directory) / "generated.rs"
        write_client(fresh)
        difference = unified_diff(CLIENT, fresh)
    if difference:
        print(difference, end="")
        fail(
            f"`{CLIENT.relative_to(REPOSITORY)}` is stale; regenerate it with `just generate`"
        )


def unified_diff(committed: Path, fresh: Path) -> str:
    """The unified diff from `committed` to `fresh`, empty when they match."""
    return "".join(
        difflib.unified_diff(
            committed.read_text(encoding="utf-8").splitlines(keepends=True),
            fresh.read_text(encoding="utf-8").splitlines(keepends=True),
            fromfile=str(committed.relative_to(REPOSITORY)),
            tofile="generated",
        )
    )
