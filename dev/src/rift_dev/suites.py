"""Run the Rust test suites: unit, live, and corpus, from sources or from an archive.

An archive is a nextest build archive CI compiled once and hands to the jobs that
run from it; without one, each suite compiles what it runs.
"""

from __future__ import annotations

import os
from pathlib import Path

from rift_dev import doctest_run, nextest_run
from rift_dev.build_run import run as run_cargo_build
from rift_dev.commands import REPOSITORY, CargoCommand
from rift_dev.config import CorpusName

# `generate-check` validates the generated client's bytes, so coverage measures
# the Rust source maintained by hand.
GENERATED_CLIENT = r"(^|/)crates/rift-cloud-client/src/generated\.rs$"
COVERAGE_FLOOR = "86"

# Every cache directory tag opens with this fixed signature, the MD5 of
# `.IsCacheDirectory` (https://bford.info/cachedir/). `cargo clean --target-dir`
# reads those 43 bytes and refuses a directory without them. Cargo writes the tag
# only when it creates a target directory itself.
CACHEDIR_TAG = (
    "Signature: 8a477f597d28d172789f06886806bc55\n"
    "# This file is a cache directory tag created by cargo.\n"
    "# For information about cache directory tags see https://bford.info/cachedir/\n"
)

LIVE_ARCHIVE = REPOSITORY / "target/live-archive"


def doctest() -> None:
    """Runs Rust documentation examples with the dev appliance collector."""
    doctest_run.run(
        CargoCommand("test", "--doc", "--workspace", "--all-features", "--locked")
    )


def coverage_target() -> None:
    """Creates the directory cargo-llvm-cov builds into and nextest extracts into.

    Nextest will not create it, so it exists before an archive run. cargo-llvm-cov
    cleans stale objects through `cargo clean`, which refuses a directory carrying
    no `CACHEDIR.TAG`, and cargo-llvm-cov only warns: a report taken over the
    uncleaned directory counts every source file twice, once from a stale object
    with no hits, and the floor fails on a green suite.
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
    """Runs unit and selected measurement tests under one coverage report.

    Unit tests use local fixtures and require no language servers or model
    downloads.
    """
    coverage_target()
    CargoCommand("llvm-cov", "clean", "--workspace").run()
    unit_sources = ["--workspace", "--all-targets", "--all-features", "--locked"]
    invocations = [
        (archive_selection(archive, unit_sources), [], {}),
        (
            archive_selection(
                archive, ["-p", "rift-tracing", "--lib", "--all-features", "--locked"]
            ),
            [
                "-E",
                "test(=recorder::tests::a_test_with_no_recorder_exports_from_sync_async_and_unwinding_contexts)",
                "-E",
                "test(=otlp::tests::trace_flush_leaves_metric_collection_for_provider_shutdown)",
                "-E",
                "test(=otlp::tests::staged_shutdown_keeps_logs_open_until_trace_and_meter_shutdown_finishes)",
                "-E",
                "test(=otlp::tests::a_stalled_collector_ends_the_shutdown_at_its_deadline)",
                "-E",
                "test(=otlp::tests::a_refusing_collector_keeps_the_span_queue_bounded_and_counts_the_drops)",
                "-E",
                "test(=otlp::tests::a_refusing_collector_keeps_the_log_queue_bounded_and_counts_the_drops)",
            ],
            {"OTEL_SDK_DISABLED": "false", "RIFT_OTLP_FILTER": "debug"},
        ),
        (
            archive_selection(
                archive,
                [
                    "-p",
                    "rift-tracing",
                    "--test",
                    "shutdown",
                    "--all-features",
                    "--locked",
                ],
            ),
            ["-E", "test(=shutdown_sends_records_before_closing_export)"],
            {"OTEL_SDK_DISABLED": "false", "RIFT_OTLP_FILTER": "debug"},
        ),
        (
            archive_selection(
                archive, ["-p", "rift-mcp", "--lib", "--all-features", "--locked"]
            ),
            [
                "--run-ignored",
                "all",
                "-E",
                "binary(=rift_mcp) and test(=output::render::tests::output_allocation_cost)",
            ],
            {"OTEL_SDK_DISABLED": "false"},
        ),
    ]
    for selection, filters, environment in invocations:
        command = CargoCommand(
            "llvm-cov",
            "nextest",
            "--no-report",
            *selection,
            "--profile",
            "ci",
            "--no-tests",
            "fail",
            *filters,
        )
        command.with_env(**environment)
        nextest_run.run(command)

    report = CargoCommand(
        "llvm-cov",
        "report",
        "--ignore-filename-regex",
        GENERATED_CLIENT,
        "--lcov",
        "--output-path",
        "lcov.info",
        "--fail-under-lines",
        COVERAGE_FLOOR,
    )
    if archive is not None:
        report.with_args("--nextest-archive-file", archive)
    report.run()


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
    nextest_run.run(
        CargoCommand(
            "nextest", "run", "--profile", "live", "--no-tests", "fail", *selection
        ).with_env(RIFT_ENGINE_LIVE="1", RIFT_LIVE_SEARCH="1")
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
    command = CargoCommand(
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
    )
    if test_name:
        command.with_args("--", "--exact", test_name)
    nextest_run.run(command)


def nextest(arguments: list[str], *, coverage: bool = False) -> None:
    """Runs a nextest suite, with optional llvm-cov coverage."""
    if coverage:
        if not arguments or arguments[0] != "run":
            raise ValueError("The --coverage option requires the run operation.")
        nextest_run.run(
            CargoCommand("llvm-cov", "nextest", "--no-report", *arguments[1:])
        )
        return
    if arguments and arguments[0] == "archive":
        run_cargo_build(arguments[1:], cargo_arguments=("nextest", "archive"))
    elif arguments and arguments[0] == "run":
        nextest_run.run(CargoCommand("nextest", *arguments))
    else:
        CargoCommand("nextest", *arguments).run()
