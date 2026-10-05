"""The artifact and agent runners start a collector, point every server at it, and
report what it received."""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator, Iterator, Sequence
from contextlib import asynccontextmanager, contextmanager
from pathlib import Path
from types import TracebackType
from typing import Any, Self

import pytest
from rift_dev import check_agent, check_artifact, rift_test_client
from rift_dev.rift_test_client import Client, Server, collector_line
from rift_dev.trace import Collector, MetricPoint

ENDPOINT = "http://127.0.0.1:4318"
NINE = 1_791_190_800 * 1_000_000_000
STARTED: list[Server] = []
POINTS: list[int] = [1]


@contextmanager
def received() -> Iterator[Collector]:
    """A collector with an endpoint and no receiver; it holds `POINTS[0]` points."""
    held = Collector(endpoint=ENDPOINT)
    for second in range(POINTS[0]):
        held.metrics.received += 1
        held.metrics.points.append(
            MetricPoint(
                "rift.requests",
                "sum",
                "1",
                "cumulative",
                (),
                (),
                NINE,
                NINE + second * 1_000_000_000,
                7,
            )
        )
    yield held


class StubServer(Server):
    """The real environment and window, with no process behind them."""

    broken = False

    def __init__(self, *arguments: Any, **keywords: Any) -> None:
        super().__init__(*arguments, **keywords)
        STARTED.append(self)

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        return None

    def read_logs(self, arguments: Sequence[str]) -> str:
        return ""

    def records(self) -> str:
        return "records"

    def read_log(self) -> str:
        return ""

    def stop(self, timeout_seconds: float = 0.0) -> None:
        return None

    @asynccontextmanager
    async def connect(
        self, *_arguments: Any, **_keywords: Any
    ) -> AsyncIterator[Client]:
        raise AssertionError("served read failed")
        yield  # pragma: no cover


@pytest.fixture(params=[check_artifact, check_agent], ids=["artifact", "agent"])
def runner(
    request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> Any:
    module = request.param
    monkeypatch.setattr(rift_test_client, "INTEGRATION_DIRECTORY", tmp_path / "kept")
    STARTED.clear()
    POINTS[:] = [1]
    monkeypatch.setattr(module, "collector", received)
    monkeypatch.setattr(module, "Server", StubServer)
    monkeypatch.setattr(module, "verify_version", lambda *_a: None)
    return module


def run(module: Any, binary: Path) -> None:
    check = getattr(module, "check_" + module.__name__.rsplit("_", 1)[1])
    asyncio.run(check(binary, "0.0.0"))


def test_the_server_environment_reaches_the_exporter(
    runner: Any, tmp_path: Path
) -> None:
    with pytest.raises(AssertionError, match="served read failed"):
        run(runner, tmp_path / "rift")
    (server,) = STARTED
    assert server.env["OTEL_EXPORTER_OTLP_ENDPOINT"] == ENDPOINT
    assert server.collector is not None


def test_a_failure_prints_the_window_points_and_the_collector_entry(
    runner: Any, tmp_path: Path
) -> None:
    with pytest.raises(AssertionError) as caught:
        run(runner, tmp_path / "rift")
    notes = "\n".join(caught.value.__notes__)
    assert "rift.requests" in notes
    assert '"points": 1, "spans": 0}' in notes


def test_a_run_without_points_still_names_the_collector_entry(
    runner: Any, tmp_path: Path
) -> None:
    POINTS[0] = 0
    with pytest.raises(AssertionError) as caught:
        run(runner, tmp_path / "rift")
    notes = "\n".join(caught.value.__notes__)
    assert "no metric points" in notes
    assert '"points": 0, "spans": 0}' in notes


def test_the_entry_is_one_json_line() -> None:
    line = collector_line(Collector())
    entry = json.loads(line.removeprefix("collector: "))
    assert (entry["points"], entry["spans"]) == (0, 0)
    assert set(entry["dropped"].values()) == {0}
    assert collector_line(None) == "collector: null"


def test_a_run_keeps_its_workspace_and_collector_counts_under_the_integration_directory(
    runner: Any, tmp_path: Path
) -> None:
    name = runner.__name__.rsplit("_", 1)[1]
    with pytest.raises(AssertionError, match="served read failed"):
        run(runner, tmp_path / "rift")
    kept = tmp_path / "kept" / name
    assert (kept / "workspace" / "rift.toml").is_file()
    counts = json.loads((kept / "collector.json").read_text(encoding="utf-8"))
    assert (counts["points"], counts["spans"]) == (1, 0)
    (server,) = STARTED
    assert server.log_path == kept / "server.log"


def test_a_run_starts_from_an_empty_directory_of_its_own(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(rift_test_client, "INTEGRATION_DIRECTORY", tmp_path / "kept")
    first = rift_test_client.retained_directory("agent")
    (first / "stale.log").write_text("old", encoding="utf-8")
    second = rift_test_client.retained_directory("agent")
    assert second == first
    assert list(second.iterdir()) == []
