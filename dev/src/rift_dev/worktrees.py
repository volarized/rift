"""Clean the Cargo build output of every Git worktree of this repository."""

from __future__ import annotations

import subprocess
from pathlib import Path

from rift_dev.commands import output

WORKTREE_LINE = "worktree "


def worktrees(porcelain: str) -> list[Path]:
    """Every worktree `git worktree list --porcelain` names."""
    return [
        Path(line.removeprefix(WORKTREE_LINE))
        for line in porcelain.splitlines()
        if line.startswith(WORKTREE_LINE)
    ]


def clean() -> None:
    """Runs `cargo clean` in every worktree holding a Cargo workspace.

    A worktree whose checkout is broken is reported and skipped, so one stale
    entry leaves the rest cleaned.
    """
    for tree in worktrees(output("git", "worktree", "list", "--porcelain")):
        manifest = tree / "Cargo.toml"
        if not manifest.is_file():
            continue
        print(f"cleaning {tree}", flush=True)
        cleaned = subprocess.run(
            ["cargo", "clean", "--manifest-path", str(manifest)], check=False
        )
        if cleaned.returncode != 0:
            print(f"skipped {tree}: broken checkout", flush=True)
