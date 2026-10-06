"""Build Rust targets with compact terminal output and a bounded retained log."""

from __future__ import annotations

import asyncio
import time
from collections.abc import Mapping, Sequence

from rift_dev.commands import REPOSITORY, CargoCommand
from rift_dev.progress import RunLabel, finish, start

BUILD_SECONDS_MAX = 1_800.0
BUILD_LOG_BYTES_MAX = 32 * 1024 * 1024
BUILD_TAIL_BYTES_MAX = 1024 * 1024
BUILD_LOG_DIRECTORY = REPOSITORY / "target" / "integration" / "build"


def run(
    arguments: Sequence[str],
    *,
    cargo_arguments: Sequence[str] = ("build",),
    environment: Mapping[str, str] | None = None,
    label: RunLabel = "build",
) -> None:
    """Run a Cargo command with compact output and a bounded retained log."""
    if any(argument in {"-h", "--help", "-V", "--version"} for argument in arguments):
        CargoCommand(*cargo_arguments, *arguments).run()
        return

    BUILD_LOG_DIRECTORY.mkdir(parents=True, exist_ok=True)
    log_path = BUILD_LOG_DIRECTORY / f"{label}-{time.time_ns()}.log"
    written = 0
    total = 0
    tail = bytearray()
    started = start(label)

    with log_path.open("wb") as output:

        def retain(chunk: bytes) -> None:
            nonlocal total, written
            total += len(chunk)
            if written < BUILD_LOG_BYTES_MAX:
                accepted = chunk[: BUILD_LOG_BYTES_MAX - written]
                output.write(accepted)
                written += len(accepted)
            tail.extend(chunk)
            if len(tail) > BUILD_TAIL_BYTES_MAX:
                del tail[: len(tail) - BUILD_TAIL_BYTES_MAX]

        async def execute() -> None:
            command = CargoCommand(*cargo_arguments, *arguments).with_timeout(
                BUILD_SECONDS_MAX
            )
            if environment is not None:
                command.with_env(**environment)
            await command.stream(retain)

        failed = False
        try:
            asyncio.run(execute())
        except Exception:
            failed = True
            if tail:
                print(tail.decode("utf-8", errors="replace"), end="")
                if not tail.endswith(b"\n"):
                    print()
            detail = f"{label} output saved to {log_path}"
            if total > written:
                detail += f"; retained first {written} of {total} bytes"
            print(detail)
            raise
        finally:
            finish(label, started, failed=failed)
