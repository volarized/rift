"""Build Rust targets with compact terminal output and a bounded retained log."""

from __future__ import annotations

import asyncio
import time

from rift_dev.commands import REPOSITORY, CargoCommand
from rift_dev.progress import finish, start

BUILD_SECONDS_MAX = 1_800.0
BUILD_LOG_BYTES_MAX = 32 * 1024 * 1024
BUILD_TAIL_BYTES_MAX = 1024 * 1024
BUILD_LOG_DIRECTORY = REPOSITORY / "target" / "integration" / "build"


def run(arguments: list[str]) -> None:
    """Run `cargo build` with `arguments`, retaining bounded output for diagnosis."""
    BUILD_LOG_DIRECTORY.mkdir(parents=True, exist_ok=True)
    log_path = BUILD_LOG_DIRECTORY / f"build-{time.time_ns()}.log"
    written = 0
    total = 0
    tail = bytearray()
    started = start("build")

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
            command = CargoCommand("build", *arguments).with_timeout(BUILD_SECONDS_MAX)
            await command.stream(retain)

        failed = False
        try:
            asyncio.run(execute())
        except Exception:
            failed = True
            if tail:
                print(tail.decode("utf-8", errors="replace"), end="")
            detail = f"build output saved to {log_path}"
            if total > written:
                detail += f"; retained first {written} of {total} bytes"
            print(detail)
            raise
        finally:
            finish("build", started, failed=failed)
