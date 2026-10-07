"""Run Rift development checks through one installed command."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import sys
import traceback
from builtins import BaseExceptionGroup
from pathlib import Path
from typing import Annotated, Any, Literal

import typer
from typer.core import TyperCommand

from rift_dev import (
    build_cache,
    build_run,
    check_agent,
    check_artifact,
    check_coldstart,
    check_corpus,
    check_dashes,
    check_mcp_conformance,
    check_rust_architecture,
    generated,
    release_tag,
    suites,
    worktrees,
)
from rift_dev.commands import CargoCommand, CommandFailed
from rift_dev.config import BinaryOptions, CorpusCase, CorpusName, CorpusOptions
from rift_dev.corpus_cache import git, measure, pins
from rift_dev.progress import finish, start
from rift_dev.rift_test_client import candidate_binary, run_gate, workspace_version

app = typer.Typer(no_args_is_help=True, pretty_exceptions_enable=False)
test_app = typer.Typer(no_args_is_help=True, pretty_exceptions_enable=False)
app.add_typer(test_app, name="test", help="Run the Rust test suites.")
ArchiveArgument = Annotated[
    Path | None, typer.Argument(help="A nextest archive CI compiled; omit to build.")
]
PathOption = Annotated[Path | None, typer.Option()]
StringOption = Annotated[str | None, typer.Option()]
FAILURE_GROUP_DEPTH_MAX = 32
PASSTHROUGH_ARGUMENTS = "rift_dev_passthrough_arguments"


class ForwardingTyperCommand(TyperCommand):
    """Retain child arguments that Click removes while parsing its separator."""

    def parse_args(self, ctx: Any, args: list[str]) -> list[str]:
        ctx.meta[PASSTHROUGH_ARGUMENTS] = list(args)
        return super().parse_args(ctx, args)


def forwarded_arguments(context: typer.Context) -> list[str]:
    """Return child arguments with Click's `--` separator preserved."""
    arguments = context.meta.pop(PASSTHROUGH_ARGUMENTS, context.args)
    if arguments and "--" in arguments:
        separator = arguments.index("--")
        if arguments[separator + 1 :] in (["--help"], ["-h"], ["--version"], ["-V"]):
            del arguments[separator]
    return list(arguments)


@app.command()
def artifact(
    binary: PathOption = None,
    target: StringOption = None,
    version: StringOption = None,
    junit: PathOption = None,
) -> None:
    """Check reads and shutdown using a supplied or compiled binary."""
    options = BinaryOptions(binary=binary, target=target, version=version, junit=junit)
    executable = candidate_binary(options.binary, options.target)
    run_gate(
        "artifact",
        check_artifact.check_artifact(
            executable, options.version or workspace_version()
        ),
        options.junit,
    )


@app.command()
def agent(
    binary: PathOption = None,
    target: StringOption = None,
    version: StringOption = None,
    junit: PathOption = None,
) -> None:
    """Exercise read tools and resources through the MCP SDK."""
    options = BinaryOptions(binary=binary, target=target, version=version, junit=junit)
    executable = candidate_binary(options.binary, options.target)
    run_gate(
        "agent",
        check_agent.check_agent(executable, options.version or workspace_version()),
        options.junit,
    )


@app.command()
def coldstart(
    binary: PathOption = None,
    target: StringOption = None,
    version: StringOption = None,
    junit: PathOption = None,
    image: Annotated[str, typer.Option()] = "ubuntu:24.04",
) -> None:
    """Check an empty workspace and later source file in a container."""
    options = BinaryOptions(binary=binary, target=target, version=version, junit=junit)
    if options.binary is None and options.target is None and sys.platform != "linux":
        raise ValueError("cold start requires --binary or --target for Linux")
    executable = candidate_binary(options.binary, options.target)
    run_gate(
        "coldstart",
        check_coldstart.check_coldstart(
            executable, image, options.version or workspace_version()
        ),
        options.junit,
    )


@app.command()
def conformance(binary: PathOption = None) -> None:
    """Run the pinned MCP conformance runner."""
    raise typer.Exit(check_mcp_conformance.main(binary))


@app.command()
def corpus(
    command: Annotated[Literal["sync", "measure", "test"], typer.Argument()],
    name: Annotated[CorpusName | None, typer.Argument()] = None,
    binary: PathOption = None,
    report: PathOption = None,
    case: Annotated[CorpusCase, typer.Option()] = "workspace",
) -> None:
    """Fetch pinned repositories, measure their trees, or run integration tests."""
    options = CorpusOptions(
        command=command, name=name, binary=binary, report=report, case=case
    )
    selected = pins()
    if options.name:
        selected = {options.name: selected[options.name]}
    for pin in selected.values():
        if options.command == "sync":
            print(pin.sync(), flush=True)
        elif options.command == "measure":
            print(
                json.dumps(
                    dataclasses.asdict(
                        measure(
                            git(
                                pin.cache, "ls-tree", "-r", "-l", "-z", pin.commit
                            ).output_bytes()
                        )
                    )
                )
            )
        else:
            assert options.binary is not None
            destination = options.report or Path(
                f"target/test-results/corpus/{pin.name}/{options.case}/report.json"
            )
            asyncio.run(
                check_corpus.Corpus(
                    pin, options.binary, destination, options.case
                ).run()
            )


@app.command("build-cache")
def start_build_cache() -> None:
    """Start sccache against the R2 build cache for the rest of a CI job."""
    raise typer.Exit(build_cache.main())


@app.command(
    "build",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def build(context: typer.Context) -> None:
    """Run `cargo build` with the given arguments and compact output."""
    build_run.run(forwarded_arguments(context))


@app.command(
    "check",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def check(context: typer.Context) -> None:
    """Run `cargo check` with the given arguments and compact output."""
    build_run.run(
        forwarded_arguments(context), cargo_arguments=("check",), label="check"
    )


@app.command(
    "clippy",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def clippy(context: typer.Context) -> None:
    """Run `cargo clippy` with the given arguments and compact output."""
    build_run.run(
        forwarded_arguments(context), cargo_arguments=("clippy",), label="clippy"
    )


@app.command(
    "docs",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def docs(context: typer.Context) -> None:
    """Run `cargo doc` with the given arguments and compact output."""
    build_run.run(
        forwarded_arguments(context),
        cargo_arguments=("doc",),
        environment={"RUSTDOCFLAGS": "-D warnings"},
        label="docs",
    )


@app.command(
    "fmt",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def format_rust(context: typer.Context) -> None:
    """Check Rust formatting with compact output."""
    started = start("format")
    failed = False
    try:
        CargoCommand("fmt", *forwarded_arguments(context)).run()
    except Exception:
        failed = True
        raise
    finally:
        finish("format", started, failed=failed)


@app.command(
    "coverage-report",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def coverage_report(context: typer.Context) -> None:
    """Write an llvm-cov report with its selected output visible."""
    CargoCommand("llvm-cov", "report", *forwarded_arguments(context)).run()


@app.command("rust-architecture")
def rust_architecture() -> None:
    """Check internal Cargo dependencies and test targets."""
    raise typer.Exit(check_rust_architecture.main())


@app.command()
def dashes(paths: Annotated[list[Path] | None, typer.Argument()] = None) -> None:
    """Check prose and source files for banned dash characters."""
    raise typer.Exit(check_dashes.main([str(path) for path in paths or []]))


@app.command()
def generate(
    check: Annotated[
        bool, typer.Option(help="Fail on a stale generated file instead of writing.")
    ] = False,
) -> None:
    """Write the schemas, analyzer manifest, API client, and CLI help transcript."""
    generated.generate(check)


@app.command()
def clean() -> None:
    """Clean the Cargo build output of every worktree of this repository."""
    worktrees.clean()


@app.command()
def release(tag: Annotated[str, typer.Argument()]) -> None:
    """Sign TAG onto the commit origin/main names and push it."""
    release_tag.release(tag)


@test_app.command("unit")
def unit_tests(archive: ArchiveArgument = None) -> None:
    """Run the unit suite under coverage, held to the line floor."""
    suites.unit(archive)


@test_app.command("doctest")
def doctest_tests() -> None:
    """Run Rust documentation examples through the dev appliance."""
    suites.doctest()


@test_app.command(
    "archive",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def archive(context: typer.Context) -> None:
    """Build a cargo-llvm-cov nextest archive with compact output."""
    build_run.run(
        forwarded_arguments(context), cargo_arguments=("llvm-cov", "nextest-archive")
    )


@test_app.command(
    "nextest-archive",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def nextest_archive(context: typer.Context) -> None:
    """Build a cargo-nextest archive with compact output."""
    build_run.run(forwarded_arguments(context), cargo_arguments=("nextest", "archive"))


@test_app.command("live")
def live_tests(archive: ArchiveArgument = None) -> None:
    """Run the live language-engine and model suites."""
    suites.live(archive)


@test_app.command(
    "nextest",
    cls=ForwardingTyperCommand,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def nextest_tests(
    context: typer.Context,
    coverage: Annotated[
        bool, typer.Option(help="Run this selection with `cargo llvm-cov nextest`.")
    ] = False,
) -> None:
    """Run `cargo nextest` with the given arguments beside the OTLP collector."""
    arguments = forwarded_arguments(context)
    separator = arguments.index("--") if "--" in arguments else len(arguments)
    arguments = [
        argument
        for argument in arguments[:separator]
        if argument not in {"--coverage", "--no-coverage"}
    ] + arguments[separator:]
    suites.nextest(arguments, coverage=coverage)


@test_app.command("corpus")
def corpus_tests(
    name: Annotated[CorpusName, typer.Argument()],
    test_name: Annotated[str, typer.Argument()] = "",
    archive: ArchiveArgument = None,
) -> None:
    """Run one pinned repository's corpus suite, or TEST_NAME alone."""
    suites.corpus(name, test_name or None, archive)


@app.command("integration-test")
def integration_test() -> None:
    """Run the corpus, live, artifact, agent, and conformance checks in turn.

    Corpus servers run one after another on a development machine.
    """
    for pin in pins().values():
        print(pin.sync(), flush=True)
    for name in ("fastapi", "bun", "nextjs"):
        suites.corpus(name, None, None)
    suites.live(None)
    artifact()
    agent()
    conformance()


def main() -> None:
    """Runs one rift-dev command, ending with a failed program's own exit status.

    A streamed program has already printed its output, so the failure adds one
    line naming the program instead of a traceback.
    """
    _use_utf8_output()
    try:
        app()
    except CommandFailed as failure:
        print(f"error: {failure}", file=sys.stderr)
        raise SystemExit(failure.status) from None
    except Exception as failure:  # noqa: BLE001 - CLI reports failures without stack frames.
        print(f"error: {failure_message(failure)}", file=sys.stderr)
        raise SystemExit(1) from None


def _use_utf8_output() -> None:
    """Use UTF-8 for Typer output, including on Windows redirected streams."""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            reconfigure(encoding="utf-8")


def failure_message(failure: BaseException) -> str:
    """Show the primary test failure; reports retain complete groups and stack frames."""
    for _ in range(FAILURE_GROUP_DEPTH_MAX):
        if not isinstance(failure, BaseExceptionGroup):
            break
        failure = failure.exceptions[0]
    return "".join(traceback.format_exception_only(failure)).rstrip()
