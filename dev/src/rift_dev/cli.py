"""Run Rift development checks through one installed command."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import sys
from pathlib import Path
from typing import Annotated, Literal

import typer

from rift_dev import (
    build_cache,
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
    trace,
    worktrees,
)
from rift_dev.commands import CommandFailed
from rift_dev.config import BinaryOptions, CorpusCase, CorpusName, CorpusOptions
from rift_dev.corpus_cache import git, measure, pins
from rift_dev.rift_test_client import candidate_binary, run_gate, workspace_version

app = typer.Typer(no_args_is_help=True, pretty_exceptions_enable=False)
test_app = typer.Typer(no_args_is_help=True, pretty_exceptions_enable=False)
app.add_typer(test_app, name="test", help="Run the Rust test suites.")
ArchiveArgument = Annotated[
    Path | None, typer.Argument(help="A nextest archive CI compiled; omit to build.")
]
PathOption = Annotated[Path | None, typer.Option()]
StringOption = Annotated[str | None, typer.Option()]


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


@app.command("rust-architecture")
def rust_architecture() -> None:
    """Check internal Cargo dependencies and test targets."""
    raise typer.Exit(check_rust_architecture.main())


@app.command()
def dashes(paths: Annotated[list[Path] | None, typer.Argument()] = None) -> None:
    """Check prose and source files for banned dash characters."""
    raise typer.Exit(check_dashes.main([str(path) for path in paths or []]))


@app.command("trace-collector")
def trace_collector(
    host: Annotated[str, typer.Option()] = "127.0.0.1",
    port: Annotated[int, typer.Option()] = 4318,
) -> None:
    """Collect OTLP/HTTP spans in memory; Ctrl-C prints one JSON line per operation."""
    trace.collect(host, port)


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


@test_app.command("live")
def live_tests(archive: ArchiveArgument = None) -> None:
    """Run the live language-engine and model suites."""
    suites.live(archive)


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
    try:
        app()
    except CommandFailed as failure:
        print(f"error: {failure}", file=sys.stderr)
        raise SystemExit(failure.status) from None
