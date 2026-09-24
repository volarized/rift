"""Run the Rust test suites: unit, live, and corpus, from sources or from an archive.

An archive is a nextest build archive CI compiled once and hands to the jobs that
run from it; without one, each suite compiles what it runs.
"""

from __future__ import annotations

import os
from pathlib import Path

from rift_dev.commands import REPOSITORY, run
from rift_dev.config import CorpusName

# `generate-check` validates the generated client's bytes, so coverage measures
# the Rust source maintained by hand.
GENERATED_CLIENT = r"(^|/)crates/rift-cloud-client/src/generated\.rs$"
COVERAGE_FLOOR = "86"

# Cargo writes a target directory's `CACHEDIR.TAG` only when it creates that
# directory itself.
CACHEDIR_TAG = (
    "Signature: 8a477f597d28d172789f06886806bc55\n"
    "# This file is a cache directory tag created by cargo.\n"
    "# For information about cache directory tags see https://bford.info/cachedir/\n"
)

LIVE_ARCHIVE = REPOSITORY / "target/live-archive"


def coverage_target() -> None:
    """Creates the directory cargo-llvm-cov builds into and nextest extracts into.

    Nextest will not create it, so it exists before an archive run. cargo-llvm-cov
    refuses to clean stale objects out of a directory carrying no `CACHEDIR.TAG`:
    a report taken over an uncleaned directory counts every source file twice,
    once from a stale object with no hits, and the floor fails on a green suite.
    """
    target = Path(
        os.environ.get(
            "CARGO_LLVM_COV_TARGET_DIR", REPOSITORY / "target/llvm-cov-target"
        )
    )
    target.mkdir(parents=True, exist_ok=True)
    tag = target / "CACHEDIR.TAG"
    if not tag.is_file():
        tag.write_text(CACHEDIR_TAG, encoding="utf-8")


def archive_selection(archive: Path | None, sources: list[str]) -> list[str]:
    """The nextest arguments that select `archive`, or `sources` without one."""
    if archive is None:
        return sources
    return [
        "--archive-file",
        str(archive),
        "--extract-overwrite",
        "--workspace-remap",
        ".",
    ]


def unit(archive: Path | None) -> None:
    """Runs the unit suite under coverage and holds it to the line floor.

    Unit tests use local fixtures and require no language servers or model
    downloads.
    """
    coverage_target()
    run(
        "cargo",
        "llvm-cov",
        "nextest",
        *archive_selection(
            archive, ["--workspace", "--all-targets", "--all-features", "--locked"]
        ),
        "--profile",
        "ci",
        "--no-tests",
        "fail",
        "--ignore-filename-regex",
        GENERATED_CLIENT,
        "--lcov",
        "--output-path",
        "lcov.info",
        "--fail-under-lines",
        COVERAGE_FLOOR,
    )


def live(archive: Path | None) -> None:
    """Runs the live suites, which drive real language engines and the model hub.

    They read the same build the unit suites do, so they reuse the fast archive
    instead of an optimized build of their own: what they exercise is the engine,
    not the speed of Rift's own code. They report no coverage. Measured against
    the unit report on one tree, the eight live tests reach three lines it does
    not, out of 78,757, so the report they add is a workspace-wide one whose lines
    are almost all zero. Uploading it made a pull request's patch figure read from
    whichever job reported first.
    """
    selection = ["--workspace", "--all-targets", "--all-features", "--locked"]
    if archive is not None:
        # Nextest extracts an archive into a directory it opens rather than
        # creates, and a missing one refuses the run with exit code 96.
        LIVE_ARCHIVE.mkdir(parents=True, exist_ok=True)
        selection = [
            "--archive-file",
            str(archive),
            "--extract-to",
            str(LIVE_ARCHIVE.relative_to(REPOSITORY)),
            "--extract-overwrite",
            "--workspace-remap",
            ".",
        ]
    run(
        "cargo",
        "nextest",
        "run",
        "--profile",
        "live",
        "--no-tests",
        "fail",
        *selection,
        env={"RIFT_ENGINE_LIVE": "1", "RIFT_LIVE_SEARCH": "1"},
    )


def corpus(name: CorpusName, test_name: str | None, archive: Path | None) -> None:
    """Runs one repository's corpus suite, or one test of it, collecting coverage."""
    coverage_target()
    binary = f"corpus_{name}"
    if archive is None:
        selection = [
            "--locked",
            "-p",
            "rift",
            "--test",
            binary,
            "--cargo-profile",
            "corpus",
        ]
    else:
        selection = [
            *archive_selection(archive, []),
            "-E",
            f"binary(={binary})",
        ]
    run(
        "cargo",
        "llvm-cov",
        "nextest",
        "--no-report",
        "--profile",
        "corpus",
        "--no-tests",
        "fail",
        "--run-ignored",
        "all",
        *selection,
        *(["--", "--exact", test_name] if test_name else []),
    )
