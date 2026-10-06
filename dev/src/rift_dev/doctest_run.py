"""Run rustdoc tests with the dev appliance collector and failure output."""

from __future__ import annotations

import asyncio
import os
import re
import time
from pathlib import Path
from typing import Final

from rift_dev.commands import REPOSITORY, Command, CommandFailed
from rift_dev.nextest_run import (
    EXIT_WAIT_SECONDS,
    last_values,
    newest,
    still_open,
    timeline,
)
from rift_dev.progress import finish, start
from rift_dev.trace import CaseStore, CaseTelemetry, Collector, collector

RUN_LOG_BYTES_MAX: Final = 16 * 1024 * 1024
RUN_TAIL_BYTES_MAX: Final = 1024 * 1024
REPORT_DIRECTORY: Final = REPOSITORY / "target" / "integration" / "doctest"
FAILED_DOC_TEST = re.compile(r"^test (.+) \.\.\. FAILED$")


def run(command: Command) -> None:
    """Run rustdoc tests, collecting OTLP records under one suite identity."""
    REPORT_DIRECTORY.mkdir(parents=True, exist_ok=True)
    log_path = REPORT_DIRECTORY / f"doctest-{time.time_ns()}.log"
    report_path = log_path.with_suffix(".txt")
    cases = CaseStore()
    tail = bytearray()
    total = 0
    retained = 0
    started = start("tests")
    status: int | None = None
    failure: Exception | None = None

    def retain(data: bytes) -> None:
        nonlocal retained, total
        total += len(data)
        if retained < RUN_LOG_BYTES_MAX:
            chunk = data[: RUN_LOG_BYTES_MAX - retained]
            with log_path.open("ab") as output:
                output.write(chunk)
            retained += len(chunk)
        tail.extend(data)
        if len(tail) > RUN_TAIL_BYTES_MAX:
            del tail[: len(tail) - RUN_TAIL_BYTES_MAX]

    with collector(cases=cases) as served:
        inherited = command.environment()
        source = os.environ if inherited is None else inherited
        environment = served.environment("cargo test --doc", source=source)
        environment["OTEL_SDK_DISABLED"] = source.get("OTEL_SDK_DISABLED", "true")
        environment["RIFT_SCOPED_RECORDER_STREAM"] = "1"
        command.with_env(**environment)
        try:
            status = asyncio.run(
                command.stream(retain, exit_wait_seconds=EXIT_WAIT_SECONDS)
            ).status
        except CommandFailed as error:
            failure = error
            status = error.status
        except Exception as error:  # noqa: BLE001 - retain failure evidence before re-raising.
            failure = error

        if failure is not None:
            telemetry = cases.take("cargo test --doc") or CaseTelemetry()
            text = tail.decode("utf-8", errors="replace")
            failed = FAILED_DOC_TEST.findall(text)
            report = _report(
                command,
                failed,
                text,
                telemetry,
                served,
                log_path,
                total,
                retained,
            )
            report_path.write_text(report, encoding="utf-8")
            print(report, end="", flush=True)
            print(f"failure report saved to {report_path}", flush=True)

    finish("tests", started, failed=failure is not None or status != 0)
    if failure is not None:
        raise failure
    if status != 0:
        raise CommandFailed(command, status or 0, "")


def _report(
    command: Command,
    failed_tests: list[str],
    output: str,
    telemetry: CaseTelemetry,
    served: Collector,
    log_path: Path,
    total: int,
    retained: int,
) -> str:
    lines = [
        "==== failed Rust documentation tests ====",
        f"command: {command}",
    ]
    for test in failed_tests:
        file, separator, name = test.partition(" - ")
        title = name if separator else test
        lines.append(f"test {title} in file {file} failed with error")
    if not failed_tests:
        lines.append("Rust documentation test command failed")
    lines.extend(
        [
            "---- cargo test output ----",
            output,
            "---- OTLP records for test.case.name=cargo test --doc ----",
            f"logs={len(telemetry.logs)} spans={len(telemetry.spans)} points={len(telemetry.points)}",
            *newest(timeline(telemetry), 3_000, "timeline lines"),
            "---- latest value of each metric series ----",
            *newest(last_values(telemetry.points), 400, "metric series"),
            "---- operations left open ----",
            *still_open(telemetry.logs, telemetry.spans),
            "---- collector ----",
            (
                f"received: {served.logs.received} logs, {served.spans.received} spans, "
                f"{served.metrics.received} points; dropped: {served.dropped().counts()}"
            ),
            f"run log: {log_path}",
        ]
    )
    if total > retained:
        lines.append(f"run log retained first {retained} of {total} bytes")
    lines.append("==== end of failed Rust documentation tests ====")
    return "\n".join(lines) + "\n"
