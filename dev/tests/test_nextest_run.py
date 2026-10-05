"""The nextest runner reports a failed test from what its processes exported."""

from __future__ import annotations

import sys
import textwrap
from pathlib import Path

import pytest
from rift_dev import nextest_run, trace
from rift_dev.commands import Command, CommandFailed
from rift_dev.trace import CaseStore, collector

# A child that sends one log record, one span, and one metric point under the test case
# its first argument names, then prints nextest's status lines and exits 100. The second
# argument selects `nothing`, which sends nothing, or `many`, which sends three log
# records.
CHILD = textwrap.dedent(
    """
    import os, sys, time, urllib.request
    from opentelemetry.proto.collector.logs.v1.logs_service_pb2 import ExportLogsServiceRequest
    from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import ExportTraceServiceRequest
    from opentelemetry.proto.collector.metrics.v1.metrics_service_pb2 import ExportMetricsServiceRequest
    from opentelemetry.proto.common.v1.common_pb2 import AnyValue, KeyValue

    endpoint = os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT", "")
    case, mode = sys.argv[1], sys.argv[2]
    now = time.time_ns()

    def resource(target):
        target.attributes.append(KeyValue(key="test.case.name", value=AnyValue(string_value=case)))
        target.attributes.append(KeyValue(key="process.pid", value=AnyValue(int_value=4242)))

    def send(path, body):
        request = urllib.request.Request(endpoint + path, data=body, method="POST",
            headers={"content-type": "application/x-protobuf"})
        urllib.request.urlopen(request, timeout=5).read()

    print("        PASS [   0.010s] (1/2) suite passing_case", flush=True)
    if mode != "nothing" and endpoint:
        logs = ExportLogsServiceRequest()
        resource_logs = logs.resource_logs.add()
        resource(resource_logs.resource)
        scope = resource_logs.scope_logs.add()
        for index in range(3 if mode == "many" else 1):
            record = scope.log_records.add()
            record.time_unix_nano = now + index
            record.severity_number = 13
            record.body.string_value = "operation opened"
            record.span_id = bytes([9]) * 8
            record.attributes.append(KeyValue(key="request_id", value=AnyValue(string_value="7")))
        send("/v1/logs", logs.SerializeToString())
        spans = ExportTraceServiceRequest()
        resource_spans = spans.resource_spans.add()
        resource(resource_spans.resource)
        span = resource_spans.scope_spans.add().spans.add()
        span.name, span.trace_id, span.span_id = "server.stop", bytes([1]) * 16, bytes([2]) * 8
        span.start_time_unix_nano, span.end_time_unix_nano = now, now + 5_000_000
        send("/v1/traces", spans.SerializeToString())
        metrics = ExportMetricsServiceRequest()
        resource_metrics = metrics.resource_metrics.add()
        resource(resource_metrics.resource)
        metric = resource_metrics.scope_metrics.add().metrics.add()
        metric.name = "sqlite.file.size"
        point = metric.gauge.data_points.add()
        point.time_unix_nano, point.as_int = now, 4096
        send("/v1/metrics", metrics.SerializeToString())
    print("        FAIL [   1.500s] (2/2) suite failing_case", flush=True)
    sys.exit(100)
    """
)


@pytest.fixture(autouse=True)
def report_directory(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Reports and window files land below the test's own directory."""
    monkeypatch.setattr(nextest_run, "REPOSITORY", tmp_path)
    monkeypatch.setattr(nextest_run, "REPORT_DIRECTORY", tmp_path / "reports")
    return tmp_path


def child(tmp_path: Path, mode: str) -> Command:
    script = tmp_path / "child.py"
    script.write_text(CHILD, encoding="utf-8")
    return Command(sys.executable, script, "run-1:suite$failing_case", mode).with_args(
        "--profile", "ci"
    )


def window(tmp_path: Path) -> None:
    """The window files the Rust harness writes for the failing case."""
    directory = tmp_path / "target/nextest/ci/failure-windows"
    directory.mkdir(parents=True)
    (directory / "run-1_suite_failing_case.window").write_text(
        "test=failing_case\nstarted_at=2026-10-05T18:01:12.743Z\n"
        "attempt=run-1:suite$failing_case\nprocess=4242 rift server start --foreground\n"
        "exit=4242 ExitStatus(0)\nended_at=2026-10-05T18:01:20.392Z\n",
        encoding="utf-8",
    )
    (directory / "run-1_suite_failing_case.4242.stderr").write_text(
        "stop stage ended stage=log drain outcome=ok\n", encoding="utf-8"
    )


def report(tmp_path: Path) -> str:
    return (tmp_path / "reports" / "suite-failing_case.txt").read_text(encoding="utf-8")


def test_a_failed_case_reports_every_section_from_what_its_process_sent(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    window(tmp_path)

    with pytest.raises(CommandFailed) as failed:
        nextest_run.run(child(tmp_path, "one"))

    assert failed.value.status == 100
    text = report(tmp_path)
    assert "==== failed test: suite failing_case ====" in text
    assert "FAIL [   1.500s] (2/2) suite failing_case" in text
    assert "slow-timeout" in text
    assert "pid=4242  log WARN  operation opened   req=7" in text
    assert "span begin server.stop" in text
    assert "sqlite.file.size" in text
    assert "---- last value of every instrument series ----" in text
    assert "---- operations opened with no end received ----" in text
    assert "process pid=4242 rift server start --foreground: exit ExitStatus(0)" in text
    assert "stop stage ended stage=log drain outcome=ok" in text
    assert "received: 1 log records, 1 spans, 1 points" in text
    assert text in capsys.readouterr().out
    assert not (tmp_path / "reports" / "suite-passing_case.txt").exists()


def test_a_failed_case_whose_processes_sent_nothing_says_so(tmp_path: Path) -> None:
    with pytest.raises(CommandFailed):
        nextest_run.run(child(tmp_path, "nothing"))

    text = report(tmp_path)
    assert "no telemetry carries this test's test.case.name" in text
    assert "no metric points" in text
    assert "no failure window file names this test" in text


def test_a_tripped_bound_is_reported(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(trace, "CASE_LOGS_MAX", 2)
    monkeypatch.setattr(nextest_run, "TIMELINE_LINES_MAX", 2)

    with pytest.raises(CommandFailed):
        nextest_run.run(child(tmp_path, "many"))

    text = report(tmp_path)
    assert "'logs': 1" in text
    assert "older timeline lines left out]" in text


def test_a_sender_without_a_case_is_counted_unattributed() -> None:
    cases = CaseStore()
    with collector(cases=cases) as served:
        served.logs.keep(
            trace.LogEntry(
                time_unix_nano=1, severity="INFO", body="b", attributes=(), resource=()
            )
        )
    assert cases.unattributed == 1
    assert trace.Collector().environment("a") == {}


@pytest.mark.parametrize(
    ("line", "found"),
    [
        (
            "        FAIL [  15.614s] (2292/4226) rift::server_cli stop_after_x",
            ("FAIL", "rift::server_cli", "stop_after_x", None),
        ),
        (
            "  TRY 2 PASS [   1.000s] (1/2) rift-mcp::proxy name",
            ("PASS", "rift-mcp::proxy", "name", None),
        ),
        (
            "        FAIL [  11.708s] [ 47/200] (1/1) rift::mcp_proxy repository_x",
            ("FAIL", "rift::mcp_proxy", "repository_x", 46),
        ),
        ("    Starting 4226 tests across 60 binaries", None),
    ],
)
def test_status_lines_parse(
    line: str, found: tuple[str, str, str, int | None] | None
) -> None:
    assert nextest_run.status_of(line) == found


def test_the_environment_names_the_case_percent_encoded() -> None:
    with collector() as served:
        environment = served.environment("run:bin$a,b")
    assert environment["OTEL_RESOURCE_ATTRIBUTES"] == "test.case.name=run:bin$a%2Cb"
    assert environment["OTEL_BLRP_SCHEDULE_DELAY"] == str(trace.EXPORT_INTERVAL_MS)


@pytest.mark.parametrize(
    ("case", "named"),
    [
        ("run:rift::server_cli$stop_x", True),
        ("run:rift::server_cli@stress-3$stop_x", False),
        ("run:rift::server_cli$stop_x_as_the_document_goes", False),
        ("run:rift::mcp_proxy$stop_x", False),
        ("stop_x", False),
    ],
)
def test_an_attempt_names_its_test(case: str, named: bool) -> None:
    assert nextest_run.names_case(case, "rift::server_cli", "stop_x") is named


def test_a_stress_attempt_names_its_iteration() -> None:
    case = "run:rift::server_cli@stress-46$stop_x"
    assert nextest_run.names_case(case, "rift::server_cli", "stop_x", 46)
    assert not nextest_run.names_case(case, "rift::server_cli", "stop_x", 45)
