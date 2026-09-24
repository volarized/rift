"""Run Rift development checks through one installed command."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import sys
from datetime import timedelta
from pathlib import Path
from typing import Annotated, Literal

import typer

from rift_dev import (
    check_agent,
    check_artifact,
    check_coldstart,
    check_corpus,
    check_dashes,
    check_mcp_conformance,
    check_rust_architecture,
    trace,
)
from rift_dev.config import BinaryOptions, CorpusCase, CorpusName, CorpusOptions
from rift_dev.corpus_cache import git, measure, pins
from rift_dev.rift_test_client import candidate_binary, run_gate, workspace_version

app = typer.Typer(no_args_is_help=True, pretty_exceptions_enable=False)
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
                        measure(git(pin.cache, "ls-tree", "-r", "-l", "-z", pin.commit))
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


@app.command("rust-architecture")
def rust_architecture() -> None:
    """Check internal Cargo dependencies and test targets."""
    raise typer.Exit(check_rust_architecture.main())


@app.command()
def dashes(paths: Annotated[list[Path] | None, typer.Argument()] = None) -> None:
    """Check prose and source files for banned dash characters."""
    raise typer.Exit(check_dashes.main([str(path) for path in paths or []]))


@app.command("trace-summary")
def trace_summary(
    base_url: Annotated[str, typer.Option()] = "http://localhost:16686",
    service: Annotated[str, typer.Option()] = "rift",
    since_seconds: Annotated[int, typer.Option()] = 3600,
    search_depth: Annotated[int, typer.Option()] = 200,
) -> None:
    """Summarize a local OTLP collector's spans for one service, one JSON line per operation."""
    trace.main(base_url, service, timedelta(seconds=since_seconds), search_depth)
