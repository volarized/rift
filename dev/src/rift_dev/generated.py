"""Write, or check, the files generated from the Rust sources.

The protocol schemas and the analyzer manifest come from `rift-schema-export`, the
global API client from `oas3-gen` over the published OpenAPI document, and
`docs/public/cli-help.txt` from the CLI's own help output.
"""

from __future__ import annotations

import difflib
import tempfile
from pathlib import Path

from rift_dev.commands import REPOSITORY, CargoCommand, Command, fail

OPENAPI = "docs/public/global-api.openapi.json"
CLIENT = REPOSITORY / "crates/rift-cloud-client/src/generated.rs"
CLI_HELP = REPOSITORY / "docs/public/cli-help.txt"

# Longest wait for one `cargo run`, which may first have to build the binary.
BUILD_SECONDS_MAX = 1800.0

# The commands whose help `docs/public/cli-help.txt` transcribes, in page order.
HELP_COMMANDS = (("--help",), ("server", "--help"), ("server", "logs", "--help"))


def schema_export(*arguments: str) -> CargoCommand:
    """`rift-schema-export`, built and run through Cargo, with `arguments`."""
    return CargoCommand("run", "-q", "-p", "rift-schema-export", "--", *arguments)


def generate(check: bool) -> None:
    """Writes every generated file, or with `check` fails on the first stale one."""
    if check:
        schema_export("--check", "docs", "plugins/claude").run()
        schema_export("--check", "--analyzer-manifest", ".").run()
        schema_export("--global-contract", "docs").run()
        check_client()
        check_cli_help()
    else:
        schema_export("docs", "plugins/claude").run()
        schema_export("--analyzer-manifest", ".").run()
        write_client(CLIENT)
        CLI_HELP.write_text(cli_help(), encoding="utf-8")


def write_client(destination: Path) -> None:
    """Generates the global API client's Rust types into `destination`."""
    Command(
        "oas3-gen",
        "generate",
        "types",
        "-q",
        "--enum-mode",
        "relaxed",
        "--no-ordered-collections",
        "-i",
        OPENAPI,
        "-o",
        destination,
    ).run()


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


def cli_help() -> str:
    """The CLI help transcript: each command line, then the help it prints."""
    return "\n".join(
        f"$ rift {' '.join(arguments)}\n"
        + CargoCommand("run", "-q", "-p", "rift", "--", *arguments)
        .with_timeout(BUILD_SECONDS_MAX)
        .output()
        for arguments in HELP_COMMANDS
    )


def check_cli_help() -> None:
    """Fails when `docs/public/cli-help.txt` no longer matches the CLI's help."""
    if CLI_HELP.read_text(encoding="utf-8") != cli_help():
        fail(
            "`docs/public/cli-help.txt` does not match the CLI help; "
            "regenerate it with `just generate`"
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
