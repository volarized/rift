"""Expose portable edit timing and optional sample inspection through rift-dev."""

from __future__ import annotations

import asyncio
import json
import shutil
from pathlib import Path
from typing import Annotated

import typer

from rift_dev.commands import Command
from rift_dev.perf import edit as measurement
from rift_dev.perf.profile import Frame, merged_tree, on_cpu, parse_sample
from rift_dev.perf.stages import StageRow, render_table

app = typer.Typer(no_args_is_help=True, pretty_exceptions_enable=False)
SAMPLE_BYTES_MAX = 64 * 1024 * 1024


@app.command()
def edit(
    corpus: Annotated[measurement.Corpus, typer.Argument()],
    binary: Annotated[
        Path, typer.Option(help="Release binary to measure; never rebuilt.")
    ],
    output: Annotated[
        Path, typer.Option(help="New JSON report, outside the disposable corpus.")
    ],
    binary_commit: Annotated[
        str | None, typer.Option(help="Source commit of the supplied binary, if known.")
    ] = None,
    steps: Annotated[int, typer.Option(min=3, max=64)] = 3,
    timeout_seconds: Annotated[float, typer.Option(min=1, max=1200)] = 120,
    startup_seconds: Annotated[float, typer.Option(min=1, max=1200)] = 900,
    run_seconds: Annotated[float, typer.Option(min=1, max=7200)] = 1800,
    sample_seconds: Annotated[
        int,
        typer.Option(
            min=0, max=300, help="Profile the first edit; zero disables profiling."
        ),
    ] = 30,
    pre_sample_seconds: Annotated[
        float,
        typer.Option(
            min=0.1,
            max=30,
            envvar="PRE_SAMPLE_SECONDS",
            help="Start profiling this many seconds before writing.",
        ),
    ] = 4,
) -> None:
    """Fetch bun or fastapi, perform 3+ one-file edits, print stages, and save JSON."""
    options = measurement.EditOptions(
        corpus=corpus,
        binary_commit=binary_commit,
        binary=binary,
        output=output,
        steps=steps,
        timeout_seconds=timeout_seconds,
        startup_seconds=startup_seconds,
        run_seconds=run_seconds,
        sample_seconds=sample_seconds,
        pre_sample_seconds=pre_sample_seconds,
    )
    asyncio.run(measurement.run(options))


@app.command()
def stages(report: Annotated[Path, typer.Argument()]) -> None:
    """Print saved stage tables and publication/read/CPU times without running Rift."""
    document = json.loads(read_bounded(report))
    for row in document["edits"]:
        print(
            f"edit {row['step']}: publication={row['publication_seconds']:.6f}s read={row['read_seconds']:.6f}s lexical={row['lexical_observed_seconds']:.6f}s cpu={row['cpu_seconds']:.6f}s"
        )
        print(render_table([StageRow(**stage) for stage in row["stage_table"]]))
    if document.get("failure"):
        print(f"measurement failed: {document['failure']}")


def read_bounded(path: Path) -> str:
    with path.open("rb") as stream:
        text = stream.read(SAMPLE_BYTES_MAX + 1)
    if len(text) > SAMPLE_BYTES_MAX:
        raise ValueError("report exceeds 64 MiB; select a smaller report")
    return text.decode("utf-8", errors="replace")


@app.command()
def profile(
    sample: Annotated[Path, typer.Argument()],
    under: Annotated[str, typer.Option()] = "",
    top: Annotated[int, typer.Option(min=1, max=200)] = 40,
    self_only: Annotated[bool, typer.Option()] = False,
    demangler: Annotated[
        Path | None, typer.Option(help="Override the demangler found on PATH.")
    ] = None,
) -> None:
    """Print inclusive or self on-CPU ticks from a macOS sample text report."""
    total, counts = on_cpu(sample_frames(sample, demangler), under, self_only=self_only)
    print(f"on-CPU ticks: {total}; inclusive rows overlap")
    for name, count in counts.most_common(top):
        print(f"{count:8d} {100 * count / max(1, total):5.1f}% {name[:200]}")


@app.command()
def tree(
    sample: Annotated[Path, typer.Argument()],
    root: Annotated[str, typer.Argument()],
    depth: Annotated[int, typer.Option(min=1, max=16)] = 3,
    min_percent: Annotated[float, typer.Option(min=0, max=100)] = 1,
    demangler: Annotated[
        Path | None, typer.Option(help="Override the demangler found on PATH.")
    ] = None,
) -> None:
    """Print a merged call tree below ROOT from a macOS sample text report."""
    total, counts = merged_tree(sample_frames(sample, demangler), root, depth)
    print(f"root {root}: {total} ticks")
    for path, count in sorted(counts.items()):
        if count * 100 >= total * min_percent:
            print(
                f"{'  ' * (len(path) - 1)}{count:8d} {100 * count / max(1, total):5.1f}% {path[-1][:200]}"
            )


def sample_frames(path: Path, demangler: Path | None = None) -> list[Frame]:
    """Demangle Rust names in one bounded invocation when a demangler is available."""
    frames = parse_sample(read_bounded(path))
    program = (
        str(demangler)
        if demangler
        else shutil.which("llvm-cxxfilt") or shutil.which("rustfilt")
    )
    symbols = sorted({frame.name for frame in frames if frame.name.startswith("_R")})
    if not program or not symbols:
        if symbols:
            print("demangler unavailable; symbols kept as printed")
        return frames
    decoded = (
        Command(program)
        .with_input(("\n".join(symbols) + "\n").encode())
        .with_timeout(30)
        .with_output_limit(SAMPLE_BYTES_MAX)
        .output()
        .splitlines()
    )
    if len(decoded) != len(symbols):
        raise ValueError("demangler returned a different number of symbols")
    names = dict(zip(symbols, decoded, strict=True))
    return [
        Frame(frame.depth, frame.count, names.get(frame.name, frame.name))
        for frame in frames
    ]
