"""Clean the Cargo build output of every Git worktree of this repository."""

from __future__ import annotations

from rift_dev import repository
from rift_dev.commands import CargoCommand, CommandFailed


def clean() -> None:
    """Runs `cargo clean` in every worktree holding a Cargo workspace.

    A worktree whose checkout is broken is reported and skipped, so one stale
    entry leaves the rest cleaned.
    """
    for tree in repository.worktrees():
        manifest = tree / "Cargo.toml"
        if not manifest.is_file():
            continue
        print(f"cleaning {tree}", flush=True)
        try:
            CargoCommand("clean", "--manifest-path", manifest).run()
        except CommandFailed:
            print(f"skipped {tree}: broken checkout", flush=True)
