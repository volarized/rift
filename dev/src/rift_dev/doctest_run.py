"""Run batched rustdoc tests with suite-scoped OTLP and bounded failure reports."""

from __future__ import annotations

import asyncio
import os
import re
import time
import uuid
from pathlib import Path

from rift_dev.commands import REPOSITORY, Command, CommandFailed
from rift_dev.nextest_run import (
    CONSOLE_BYTES_MAX,
    CONSOLE_EVIDENCE_MAX,
    EVIDENCE,
    EXIT_WAIT_SECONDS,
    RUN_LOG_BYTES_MAX,
    RUN_SECONDS_MAX,
    ArtifactStore,
    CollectionSummary,
    LogEvidence,
    MetricEvidence,
    SpanEvidence,
    still_open,
    success_line,
    timeline,
)
from rift_dev.progress import start
from rift_dev.trace import (
    CaseStore,
    CaseTelemetry,
    Dropped,
    LogEntry,
    MetricPoint,
    SpanRecord,
    collector,
)

REPORT_DIRECTORY = REPOSITORY / "target/integration/doctest"
FAILED_DOC_TEST = re.compile(r"^test (.+) \.\.\. FAILED$", re.MULTILINE)
RESULT = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
    re.MULTILINE,
)
FAILURES_MAX = 512
SUITE = "cargo test --doc"


class DoctestSummary(CollectionSummary):
    """Rustdoc results retain suite identity independently of optional signals."""

    command: str
    status: int | None
    elapsed_seconds: float
    launched: int
    failed_examples: tuple[str, ...]
    output_bytes_omitted: int
    telemetry_identity: str = SUITE


def telemetry_of(path: Path) -> CaseTelemetry:
    """Read the suite's original JSONL records through the shared Pydantic model."""
    telemetry = CaseTelemetry()
    with path.open("rb") as source:
        for line in source:
            row = EVIDENCE.validate_json(line)
            if isinstance(row, LogEvidence):
                if len(telemetry.logs) == telemetry.logs.maxlen:
                    telemetry.dropped.logs += 1
                telemetry.logs.append(row.record)
            elif isinstance(row, SpanEvidence):
                if len(telemetry.spans) == telemetry.spans.maxlen:
                    telemetry.dropped.spans += 1
                telemetry.spans.append(row.record)
            elif isinstance(row, MetricEvidence):
                if len(telemetry.points) == telemetry.points.maxlen:
                    telemetry.dropped.points += 1
                telemetry.points.append(row.record)
    return telemetry


def suite_report(
    summary: DoctestSummary,
    output: str,
    telemetry: CaseTelemetry,
    artifact: ArtifactStore,
) -> str:
    """Keep exact failed names and original records under suite identity."""
    lines = [
        "==== Rust documentation tests ====",
        *[f"{name} failed!" for name in summary.failed_examples],
        "  Error:",
        output,
        "  Suite telemetry: cargo test --doc",
        "  Logs:",
    ]
    for entry in telemetry.logs:
        lines.extend(
            [
                entry.line(),
                f"    resource: {entry.resource} trace={entry.trace_id} span={entry.span_id}",
            ]
        )
    lines.append("  Metrics:")
    for point in telemetry.points:
        lines.extend([point.line(), f"    resource: {point.resource}"])
    if telemetry.spans:
        lines.append("  Traces:")
        for span in telemetry.spans:
            lines.extend(
                [
                    span.line(),
                    f"    trace={span.trace_id} span={span.span_id} parent={span.parent_span_id} status={span.status_code} {span.status_message} resource={span.resource}",
                ]
            )
    lines.extend(still_open(telemetry.logs, telemetry.spans))
    lines.extend(
        [
            f"  Report omissions: {telemetry.dropped.counts()}",
            summary.model_dump_json(indent=2),
            f"  Artifacts: {artifact.path.parent}",
        ]
    )
    return "\n".join(lines) + "\n"


def run(command: Command) -> None:
    """Run one batched rustdoc command and preserve suite evidence before cleanup."""
    directory = REPORT_DIRECTORY / f"doctest-{uuid.uuid4()}"
    directory.mkdir(parents=True)
    started = start("tests")
    artifact = ArtifactStore(directory / "telemetry.jsonl", {})
    errors: list[str] = []
    status: int | None = None
    failure: BaseException | None = None
    totals = [0, 0]
    kept = [0, 0]

    def observe(record: LogEntry | SpanRecord | MetricPoint) -> bool:
        artifact.record(record)
        return False

    paths = (directory / "stdout.log", directory / "stderr.log")
    with paths[0].open("wb") as stdout, paths[1].open("wb") as stderr:

        def retain(data: bytes, index: int) -> None:
            totals[index] += len(data)
            accepted = data[: max(0, RUN_LOG_BYTES_MAX - kept[index])]
            (stdout if index == 0 else stderr).write(accepted)
            kept[index] += len(accepted)

        served_dropped = Dropped()
        request_cache_evictions = 0
        try:
            source = command.environment() or os.environ
            if source.get("OTEL_SDK_DISABLED", "").lower() == "true":
                raise ValueError(
                    "appliance collection requires OTEL_SDK_DISABLED=false"
                )
            with collector(
                cases=CaseStore(observe=observe), request_observer=artifact.request
            ) as served:
                command.with_env(
                    **served.environment(SUITE, source=source),
                    RIFT_SCOPED_RECORDER_STREAM="1",
                )
                if command.timeout_seconds is None:
                    command.with_timeout(RUN_SECONDS_MAX)
                try:
                    status = asyncio.run(
                        command.stream(
                            lambda data: retain(data, 0),
                            stderr=lambda data: retain(data, 1),
                            exit_wait_seconds=EXIT_WAIT_SECONDS,
                        )
                    ).status
                except BaseException as error:  # noqa: BLE001 - write artifacts before cleanup.
                    failure = error
                    if isinstance(error, CommandFailed):
                        status = error.status
                    else:
                        errors.append(f"incomplete execution: {error}")
            served_dropped = served.dropped()
            request_cache_evictions = served.requests.dropped
        except BaseException as error:  # noqa: BLE001 - retain setup and collection errors.
            failure = failure or error
            errors.append(f"appliance error: {error}")
        finally:
            artifact.close()
    output = "\n".join(
        path.read_text(encoding="utf-8", errors="replace") for path in paths
    )
    results = RESULT.findall(output)
    launched = sum(int(passed) + int(failed) for _, passed, failed, _ in results)
    failed_names = FAILED_DOC_TEST.findall(output)
    omitted = sum(totals) - sum(kept)
    if not launched:
        errors.append("no documentation tests launched")
    if artifact.omitted or artifact.request_errors or omitted:
        errors.append(
            f"collection omitted={artifact.omitted} requests_failed={artifact.request_errors} output_bytes_omitted={omitted}"
        )
    if artifact.write_error:
        errors.append(f"artifact write error: {artifact.write_error}")
    if served_dropped.bodies or served_dropped.kinds:
        errors.append(f"collector refused data: {served_dropped.counts()}")
    if len(failed_names) > FAILURES_MAX:
        errors.append(f"failed example names exceeded {FAILURES_MAX}")
    summary = DoctestSummary(
        command=str(command),
        status=status,
        elapsed_seconds=time.monotonic() - started,
        launched=launched,
        failed_examples=tuple(failed_names[:FAILURES_MAX]),
        errors=tuple(errors),
        received=artifact.received,
        retained=artifact.retained,
        received_bytes=artifact.received_bytes,
        request_cache_evictions=request_cache_evictions,
        omitted=artifact.omitted,
        cache_evictions=served_dropped,
        output_bytes_omitted=omitted,
    )
    (directory / "run.json").write_text(
        summary.model_dump_json(indent=2) + "\n", encoding="utf-8"
    )
    telemetry = telemetry_of(artifact.path)
    if (
        failure is not None
        or status != 0
        or errors
        or any(result == "FAILED" for result, *_ in results)
    ):
        report = suite_report(summary, output, telemetry, artifact)
        (directory / "failure.txt").write_text(report, encoding="utf-8")
        console = "\n".join(
            [
                *[f"{name} failed!" for name in summary.failed_examples],
                "  Error:",
                output[-CONSOLE_BYTES_MAX // 2 :],
                *timeline(telemetry)[-CONSOLE_EVIDENCE_MAX:],
                *errors,
                f"  Artifacts: {directory}",
            ]
        )
        print(
            console.encode("utf-8")[:CONSOLE_BYTES_MAX].decode(
                "utf-8", errors="replace"
            ),
            flush=True,
        )
        if failure is not None:
            raise failure
        raise CommandFailed(command, status or 1, f"evidence: {directory}")
    print(success_line(launched, summary.elapsed_seconds), flush=True)
