"""Run the external commands a development recipe is made of.

Each command runs from the repository root, echoed the way `just` echoes a recipe
line, and a failing one ends the recipe with that command's exit status.
"""

from __future__ import annotations

import os
import shlex
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parents[3]

Argument = str | Path


def run(*argv: Argument, env: Mapping[str, str] | None = None) -> None:
    """Runs one command and ends the recipe with its status when it fails."""
    command = [str(argument) for argument in argv]
    print(shlex.join(command), file=sys.stderr, flush=True)
    completed = subprocess.run(
        command, cwd=REPOSITORY, env=environment(env), check=False
    )
    if completed.returncode != 0:
        raise SystemExit(completed.returncode)


def output(*argv: Argument) -> str:
    """Runs one command and returns what it printed to stdout."""
    command = [str(argument) for argument in argv]
    completed = subprocess.run(
        command, cwd=REPOSITORY, stdout=subprocess.PIPE, text=True, check=False
    )
    if completed.returncode != 0:
        raise SystemExit(completed.returncode)
    return completed.stdout


def fail(message: str) -> None:
    """Ends the recipe with status 1 after printing `message` as an error."""
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def environment(extra: Mapping[str, str] | None) -> dict[str, str] | None:
    """The process environment with `extra` laid over it, or `None` to inherit it."""
    if not extra:
        return None
    return {**os.environ, **extra}
