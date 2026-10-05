"""A failure keeps the records, metric points, and spans between its lower bound and the
failure."""

from __future__ import annotations

import os
from collections.abc import Sequence
from pathlib import Path

import pytest
from rift_dev import rift_test_client
from rift_dev.rift_test_client import (
    EVIDENCE_TAIL_BYTES,
    NO_POINTS,
    NO_SPANS,
    RECORD_TAIL,
    WINDOW_POINTS_MAX,
    LogsReader,
    Server,
    failure_window,
    logs_arguments,
    telemetry_notes,
)
from rift_dev.trace import Collector, MetricPoint, SpanRecord

SINCE = "2026-10-05T09:00:00.000+00:00"
UNTIL = "2026-10-05T09:00:30.000+00:00"
IN_FLIGHT = (
    "2026-10-05 08:59:00.000Z INFO  rift_tracing::flight   in_flight=2 reason=stop  "
    "operations in flight"
)
INSIDE = (
    "2026-10-05 09:00:10.000Z ERROR rift_mcp::validation::prepare   component=index "
    "operation=index.build  build failed"
)


def test_window_arguments_name_kind_bounds_and_tail() -> None:
    assert logs_arguments(UNTIL, since=SINCE) == [
        "server",
        "logs",
        "--tail",
        str(RECORD_TAIL),
        "--since",
        SINCE,
        "--until",
        UNTIL,
    ]
    assert "--since" not in logs_arguments(UNTIL)


def reads(window: str, before: str) -> tuple[list[Sequence[str]], LogsReader]:
    seen: list[Sequence[str]] = []

    def read(arguments: Sequence[str]) -> str:
        seen.append(arguments)
        return before if "--since" not in arguments else window

    return seen, read


def test_the_window_reads_from_the_lower_bound_and_the_newest_before_it() -> None:
    seen, read = reads(INSIDE + "\n", IN_FLIGHT + "\n")
    notes = failure_window(
        read,
        since=SINCE,
        lower_bound="the end of the last recorded action, publication",
        until=UNTIL,
    )
    assert [("--since" in arguments, arguments[-1]) for arguments in seen] == [
        (True, UNTIL),
        (False, UNTIL),
    ]
    assert notes[0] == (
        f"failure window: log records from {SINCE} (the end of the last "
        f"recorded action, publication) until {UNTIL}, the newest {RECORD_TAIL} at "
        f"most\n{INSIDE}\n"
    )
    assert notes[1] == (
        f"newest operations in flight record before {UNTIL}:\n{IN_FLIGHT}"
    )
    assert len(notes) == 2


def test_a_full_window_states_that_older_records_were_left_out() -> None:
    _, read = reads((INSIDE + "\n") * RECORD_TAIL, "")
    notes = failure_window(
        read,
        since=SINCE,
        lower_bound="the server's start",
        until=UNTIL,
    )
    assert (
        f"[the window holds {RECORD_TAIL} records; older records of the window were left out]"
        in notes[0]
    )


def test_a_long_window_keeps_its_newest_bytes_and_names_the_file(
    tmp_path: Path,
) -> None:
    big = "x" * (EVIDENCE_TAIL_BYTES + 10) + "\nnewest\n"
    _, read = reads(big, "")
    file = tmp_path / "w.log"
    notes = failure_window(
        read,
        since=SINCE,
        lower_bound="the server's start",
        until=UNTIL,
        file=file,
    )
    assert f"earlier bytes are in {file}]" in notes[0]
    assert notes[0].endswith("newest\n")


def test_an_empty_window_and_absent_records_are_stated() -> None:
    _, read = reads("", "")
    notes = failure_window(
        read,
        since=None,
        lower_bound="the server's start",
        until=UNTIL,
    )
    assert "from the oldest kept record (the server's start)" in notes[0]
    assert notes[0].endswith("(no records in the window)\n")
    assert notes[1].endswith(f"(none among the newest {RECORD_TAIL} records)")
    assert len(notes) == 2


def test_a_failed_read_is_a_note_and_never_raises() -> None:
    def read(arguments: Sequence[str]) -> str:
        raise RuntimeError("rift exited 3")

    notes = failure_window(
        read, since=SINCE, lower_bound="the server's start", until=UNTIL
    )
    assert notes[0].endswith("unavailable: rift exited 3")
    assert notes[1] == ("newest operations in flight unavailable: rift exited 3")
    assert len(notes) == 2


WINDOW_BINARY = """
import os, sys
with open(os.environ["RECORD_ARGUMENTS"], "a") as seen:
    seen.write(" ".join(sys.argv[1:]) + "\\n")
print("2026-10-05 09:00:10.000Z ERROR rift_mcp::validation::prepare   component=index  build failed")
"""


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_a_server_window_starts_at_its_start_and_is_kept_in_a_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    seen = tmp_path / "arguments"
    monkeypatch.setenv("RECORD_ARGUMENTS", str(seen))
    binary = tmp_path / "rift"
    import sys

    binary.write_text(f"#!{sys.executable}\n" + WINDOW_BINARY, encoding="utf-8")
    binary.chmod(0o755)
    server = Server(binary, root, tmp_path / "server.log")
    server.started_at = SINCE
    notes = server.window()
    first, second = seen.read_text().splitlines()
    assert f"--since {SINCE} --until " in first
    assert "--since" not in second
    assert notes[0].startswith(
        f"failure window: log records from {SINCE} (the server's start)"
    )
    assert "build failed" in server.window_path.read_text()
    assert server.window_path == tmp_path / "server.window.log"
    later = server.window("2026-10-05T09:00:20.000+00:00", "a later bound")
    assert "from 2026-10-05T09:00:20.000+00:00 (a later bound)" in later[0]


NINE = 1_791_190_800 * 1_000_000_000
REQUEST_RECORD = (
    "2026-10-05 09:00:05.000Z INFO  rift_mcp::server::RiftMcp::nodes   component=mcp "
    "operation=tools/call req=7 tool=nodes  read answered"
)


def point(name: str, second: int, value: float) -> MetricPoint:
    return MetricPoint(
        name,
        "sum",
        "1",
        "cumulative",
        (("outcome", "ok"),),
        (("service.name", "rift"),),
        NINE,
        NINE + second * 1_000_000_000,
        value,
    )


def filled() -> Collector:
    held = Collector()
    for kept in (
        point("rift.late", 40, 9),
        point("rift.b", 20, 2),
        point("rift.a", 10, 1),
    ):
        held.metrics.received += 1
        held.metrics.points.append(kept)
    for name, request, end in (("mcp.request", "7", 6), ("index.build", None, 7)):
        held.spans.keep(
            SpanRecord(
                name,
                "01" * 16,
                "02" * 8,
                NINE + end * 1_000_000_000 - 2_500_000,
                NINE + end * 1_000_000_000,
                (("request_id", request),) if request is not None else (),
            )
        )
    return held


def test_a_point_and_a_span_print_in_the_layout_of_a_record() -> None:
    assert point("rift.requests", 10, 6).line() == (
        "2026-10-05 09:00:10.000Z sum       rift.requests   outcome=ok  value=6 "
        "unit=1 cumulative"
    )
    histogram = MetricPoint(
        "rift.mcp.request.duration",
        "histogram",
        "s",
        "cumulative",
        (),
        (),
        NINE,
        NINE,
        0.25,
        3,
        (0.1, 1.0),
        (2, 0, 1),
    )
    assert histogram.line() == (
        "2026-10-05 09:00:00.000Z histogram rift.mcp.request.duration   count=3 "
        "sum=0.25 buckets=<=0.1:2,>1:1 unit=s cumulative"
    )


def test_the_window_prints_points_and_joins_spans_to_records_by_request() -> None:
    _, read = reads(REQUEST_RECORD + "\n", "")
    notes = failure_window(
        read,
        since=SINCE,
        lower_bound="the server's start",
        until=UNTIL,
        collector=filled(),
    )
    assert len(notes) == 4
    assert notes[2] == (
        f"metric points from {SINCE} until {UNTIL}, the newest {WINDOW_POINTS_MAX} at "
        "most:\n"
        "2026-10-05 09:00:10.000Z sum       rift.a   outcome=ok  value=1 unit=1 "
        "cumulative\n"
        "2026-10-05 09:00:20.000Z sum       rift.b   outcome=ok  value=2 unit=1 "
        "cumulative"
    )
    assert notes[3].endswith(
        ":\n2026-10-05 09:00:06.000Z span      mcp.request   request_id=7  "
        "elapsed=2.500ms records=1\n"
        "2026-10-05 09:00:07.000Z span      index.build   elapsed=2.500ms"
    )


def test_the_window_keeps_its_newest_points_and_states_drops(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(rift_test_client, "WINDOW_POINTS_MAX", 1)
    held = filled()
    held.metrics.dropped.points = 4
    notes = telemetry_notes(held, since=SINCE, until=UNTIL)
    assert notes[0].splitlines()[1:] == [
        "[1 older points of the window were left out]",
        (
            "2026-10-05 09:00:20.000Z sum       rift.b   outcome=ok  value=2 unit=1 "
            "cumulative"
        ),
    ]
    assert notes[-1] == "the collector's bounds dropped: points=4"
    empty = telemetry_notes(held, since=UNTIL, until="2026-10-05T09:00:31.000+00:00")
    assert empty[0].endswith("(no metric points in the window; 3 received in all)")


def test_a_collector_that_received_nothing_says_so_once() -> None:
    _, read = reads("", "")
    notes = failure_window(
        read,
        since=SINCE,
        lower_bound="the server's start",
        until=UNTIL,
        collector=Collector(),
    )
    assert notes[2:] == [NO_POINTS, NO_SPANS]
    assert sum(note.count(NO_POINTS) for note in notes) == 1


def test_a_server_points_its_process_at_the_collector(tmp_path: Path) -> None:
    served = Collector(endpoint="http://127.0.0.1:4318")
    server = Server(
        tmp_path / "rift",
        tmp_path / "workspace",
        tmp_path / "server.log",
        env={"OTEL_METRIC_EXPORT_INTERVAL": "50"},
        collector=served,
    )
    assert server.env["OTEL_EXPORTER_OTLP_ENDPOINT"] == "http://127.0.0.1:4318"
    assert server.env["OTEL_METRIC_EXPORT_INTERVAL"] == "50"
    assert server.collector is served


def test_a_span_line_leaves_out_the_attributes_every_span_carries() -> None:
    span = SpanRecord(
        "mcp.request",
        "01" * 16,
        "02" * 8,
        NINE,
        NINE + 1_000_000,
        (
            ("busy_ns", "1351125"),
            ("code.file.path", "crates/rift-mcp/src/server.rs"),
            ("request_id", "1"),
            ("target", "rift_mcp::server"),
            ("thread.name", "tokio-rt-worker"),
            ("tool", "get_symbol"),
        ),
    )
    assert span.line(2) == (
        "2026-10-05 09:00:00.001Z span      mcp.request   request_id=1 tool=get_symbol  "
        "elapsed=1.000ms records=2"
    )
