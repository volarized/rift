"""Corpus commands expose tested program diagnostics and retain harness evidence."""

from __future__ import annotations

import asyncio
import io
import json
import re
import sys
from builtins import ExceptionGroup
from collections.abc import AsyncIterator, Buffer
from contextlib import asynccontextmanager
from pathlib import Path
from typing import TextIO
from unittest.mock import AsyncMock, Mock

import pytest
import tomllib
from mcp import ClientSession
from rift_dev import cli, commands, rift_test_client
from rift_dev.check_corpus import Corpus
from rift_dev.commands import Command, CommandFailed, Process
from rift_dev.corpus_cache import pins
from rift_dev.machine import machine, machine_line
from rift_dev.rift_test_client import (
    LOG_BYTES_MAX,
    Client,
    Server,
    cut_notice,
    stderr_log,
)


@pytest.mark.parametrize("fails", [False, True])
def test_actions_and_complete_failure_stay_in_report(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
    fails: bool,
) -> None:
    report = tmp_path / "report.json"
    corpus = Corpus(pins()["fastapi"], tmp_path / "rift", report)

    async def tree(_directory: Path) -> None:
        corpus.record("publication", symbols=200)
        if fails:
            raise AssertionError("source revision differs")

    monkeypatch.setattr(corpus, "tree", tree)
    if fails:
        with pytest.raises(AssertionError, match="source revision differs"):
            asyncio.run(corpus.run())
    else:
        asyncio.run(corpus.run())

    output = capsys.readouterr()
    assert output.out == machine_line(machine()) + "\n"
    assert output.err == ""
    retained = json.loads(report.read_text())
    assert retained["actions"][0]["symbols"] == 200
    assert retained["status"] == ("failed" if fails else "passed")
    if fails:
        assert "Traceback" in retained["failure"]
        assert "AssertionError: source revision differs" in retained["failure"]
    else:
        assert retained["failure"] == ""


def test_corpus_foreground_output_is_retained_and_a_pass_prints_none(
    tmp_path: Path, capfdbinary: pytest.CaptureFixture[bytes]
) -> None:
    corpus = Corpus(pins()["fastapi"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path / "workspace"
    corpus.root.mkdir()
    server = corpus.server()
    program = "import os; os.write(1,b'index publication complete\\n'); os.write(2,b'error[readiness]: source revision differs\\n')"
    with Command(sys.executable, "-c", program).spawn() as process:
        server.process = process
        server._drain()
        assert process.wait(5) == 0
    server._check_log()
    retained = server.log_path.read_bytes()
    assert b"index publication complete\n" in retained
    assert b"error[readiness]: source revision differs\n" in retained
    # A pass copies nothing to the console; a failure prints its evidence instead.
    assert capfdbinary.readouterr().err == b""


def test_sdk_stderr_is_forwarded_and_retained(
    tmp_path: Path, capfdbinary: pytest.CaptureFixture[bytes]
) -> None:
    import anyio

    path = tmp_path / "proxy.log"
    program = "import os; os.write(2,b'proxy connected to server\\n')"

    async def operation() -> None:
        with stderr_log(path, output=sys.stderr.buffer) as log:
            await anyio.run_process([sys.executable, "-c", program], stderr=log)

    asyncio.run(operation())
    assert path.read_bytes() == b"proxy connected to server\n"
    assert capfdbinary.readouterr().err == path.read_bytes()


def test_forwarded_sdk_stderr_stops_at_existing_byte_bound(tmp_path: Path) -> None:
    import anyio

    path = tmp_path / "proxy.log"
    output = io.BytesIO()
    program = f"import os; os.write(2,b'x'*{LOG_BYTES_MAX + 1})"

    async def operation() -> None:
        with stderr_log(path, output=output) as log:
            with anyio.fail_after(5):
                await anyio.run_process([sys.executable, "-c", program], stderr=log)

    with pytest.raises(RuntimeError, match="SDK stderr collection failed"):
        asyncio.run(operation())
    assert len(output.getvalue()) == LOG_BYTES_MAX
    notice = cut_notice("SDK stderr", LOG_BYTES_MAX).encode()
    assert path.read_bytes() == output.getvalue() + notice


def test_forwarded_foreground_output_stops_at_existing_byte_bound(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    maximum = 65536
    monkeypatch.setattr(rift_test_client, "LOG_BYTES_MAX", maximum)
    output = io.BytesIO()
    server = Server(
        tmp_path / "rift",
        tmp_path / "workspace",
        tmp_path / "server.log",
        output=output,
    )
    program = f"import os; os.write(1,b'x'*{maximum + 1})"
    with Command(sys.executable, "-c", program).spawn() as process:
        server.process = process
        server._drain()
        assert process.wait(5) == 0
    with pytest.raises(RuntimeError, match="server log collection failed"):
        server.check_running()
    notice = cut_notice("server output", maximum).encode()
    assert server.log_path.read_bytes() == b"x" * maximum + notice
    assert output.getvalue() == b"x" * maximum


def test_corpus_connection_keeps_proxy_diagnostics_and_prints_none(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capfdbinary: pytest.CaptureFixture[bytes],
) -> None:
    corpus = Corpus(pins()["fastapi"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path / "workspace"
    server = corpus.server()
    server.process = Mock(spec=Process)
    server.process.poll.return_value = None
    server.process.returncode = None

    @asynccontextmanager
    async def transport(
        *args: object, errlog: TextIO, **kwargs: object
    ) -> AsyncIterator[tuple[None, None]]:
        errlog.write("proxy connected to server\n")
        errlog.flush()
        yield None, None

    session = AsyncMock(spec=ClientSession)
    monkeypatch.setattr(rift_test_client, "stdio_client", transport)
    monkeypatch.setattr(rift_test_client, "ClientSession", lambda *args: session)
    monkeypatch.setattr(Client, "initialize", AsyncMock())

    async def operation() -> None:
        async with server.connect():
            pass

    asyncio.run(operation())
    retained = server.log_path.with_suffix(".mcp.log").read_bytes()
    assert retained == b"proxy connected to server\n"
    assert capfdbinary.readouterr().err == b""


@pytest.mark.parametrize("sdk", [False, True])
def test_output_failure_retains_collected_diagnostics(
    tmp_path: Path, sdk: bool
) -> None:
    class ClosedOutput(io.BytesIO):
        def write(self, buffer: Buffer, /) -> int:
            raise BrokenPipeError("test output closed")

    path = tmp_path / "server.log"
    output = ClosedOutput()
    diagnostic = b"index publication complete\n"
    if sdk:
        with (
            pytest.raises(RuntimeError, match="SDK stderr collection failed"),
            stderr_log(path, output=output) as log,
        ):
            log.write(diagnostic.decode())
            log.flush()
    else:
        server = Server(tmp_path / "rift", tmp_path / "workspace", path, output=output)
        with Command(
            sys.executable, "-c", "print('index publication complete')"
        ).spawn() as process:
            server.process = process
            server._drain()
            assert process.wait(5) == 0
        with pytest.raises(RuntimeError, match="server log collection failed"):
            server.check_running()
    assert path.read_bytes() == diagnostic


@pytest.mark.parametrize("grouped", [False, True])
def test_cli_failure_reports_reason_without_python_frames(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    grouped: bool,
) -> None:
    failure = AssertionError("source revision differs")
    failure.add_note("expected revision 2, received revision 1")
    error = ExceptionGroup("transport cleanup", [failure]) if grouped else failure

    def fail() -> None:
        raise error

    monkeypatch.setattr(cli, "app", fail)
    with pytest.raises(SystemExit) as status:
        cli.main()
    assert status.value.code == 1
    assert capsys.readouterr().err == (
        "error: AssertionError: source revision differs\n"
        "expected revision 2, received revision 1\n"
    )


def test_cli_command_failure_preserves_cargo_status(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fail() -> None:
        raise CommandFailed(Command("cargo", "nextest", "run"), 100, "")

    monkeypatch.setattr(cli, "app", fail)
    with pytest.raises(SystemExit) as status:
        cli.main()
    assert status.value.code == 100
    assert capsys.readouterr().err == "error: cargo exited 100\n"


def test_primary_failure_selection_respects_group_depth_bound() -> None:
    failure = AssertionError("source revision differs")
    failure.add_note("expected revision 2, received revision 1")
    for _ in range(cli.FAILURE_GROUP_DEPTH_MAX):
        failure = ExceptionGroup("transport cleanup", [failure])
    assert cli.failure_message(failure) == (
        "AssertionError: source revision differs\nexpected revision 2, received revision 1"
    )
    assert cli.failure_message(ExceptionGroup("outer cleanup", [failure])) == (
        "ExceptionGroup: transport cleanup (1 sub-exception)"
    )


def test_corpus_profile_displays_passing_output_and_retains_capture() -> None:
    configuration = tomllib.loads(
        (commands.REPOSITORY / ".config/nextest.toml").read_text()
    )["profile"]["corpus"]
    assert configuration["success-output"] == "immediate"
    assert configuration["failure-output"] == "immediate-final"
    assert configuration["junit"]["path"] == "junit.xml"


def corpus_with_server(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fails: bool):
    report = tmp_path / "out" / "report.json"
    corpus = Corpus(pins()["fastapi"], tmp_path / "rift", report)
    server = corpus.server(tmp_path / "workspace")
    notes = ["server stderr (x):\nboom\n", "persisted log records (y):\nERROR index\n"]
    evidence = Mock(return_value=notes)
    monkeypatch.setattr(server, "evidence", evidence)

    async def cases(_directory: Path) -> None:
        corpus.record("publication", symbols=200)
        if fails:
            raise AssertionError("source revision differs")

    monkeypatch.setattr(corpus, "cases", cases)
    return corpus, report, evidence


def test_a_passing_case_collects_and_prints_no_evidence(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
) -> None:
    corpus, report, evidence = corpus_with_server(tmp_path, monkeypatch, fails=False)
    asyncio.run(corpus.run())
    evidence.assert_not_called()
    assert capfd.readouterr().err == ""
    assert json.loads(report.read_text())["evidence"] == []


def test_a_failing_case_keeps_each_servers_evidence_in_report_and_stderr(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
) -> None:
    corpus, report, evidence = corpus_with_server(tmp_path, monkeypatch, fails=True)
    with pytest.raises(AssertionError, match="source revision differs"):
        asyncio.run(corpus.run())
    # The failing action began at or after the end of the last recorded action.
    since = corpus.mark
    bound = (
        "the end of the last recorded action, publication; the failing action "
        "began at or after it"
    )
    evidence.assert_called_once_with(since, bound)
    err = capfd.readouterr().err
    assert "server stderr (x):\nboom\n" in err
    assert "persisted log records (y):\nERROR index\n" in err
    server = corpus.servers[0]
    assert json.loads(report.read_text())["evidence"] == [
        {
            "server": 1,
            "root": str(tmp_path / "workspace"),
            "stderr": str(server.log_path),
            "stderr_cut": False,
            "proxy_stderr": [],
            "records": str(server.records_path),
            "window": str(server.window_path),
            "window_since": since,
            "window_lower_bound": bound,
        }
    ]
    assert server.records_path == tmp_path / "out" / "report.server-1.records.log"
    assert server.window_path == tmp_path / "out" / "report.server-1.window.log"


def test_a_failing_case_reaches_servers_through_the_served_tree(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Evidence is read inside the temporary tree, before the case removes it."""
    corpus, _, _ = corpus_with_server(tmp_path, monkeypatch, fails=True)
    existed: list[bool] = []
    monkeypatch.setattr(
        corpus.servers[0],
        "evidence",
        lambda *_: existed.append(corpus.root.is_dir()) or [],
    )

    async def cases(directory: Path) -> None:
        corpus.root = directory / "workspace"
        corpus.root.mkdir()
        raise AssertionError("late")

    monkeypatch.setattr(corpus, "cases", cases)
    with pytest.raises(AssertionError, match="late"):
        asyncio.run(corpus.run())
    assert existed == [True]


def test_actions_carry_utc_start_and_end(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    corpus, report, _ = corpus_with_server(tmp_path, monkeypatch, fails=False)
    asyncio.run(corpus.run())
    first = json.loads(report.read_text())["actions"][0]
    pattern = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}\+00:00"
    assert re.fullmatch(pattern, first["started_at"])
    assert re.fullmatch(pattern, first["ended_at"])
    assert first["started_at"] <= first["ended_at"]
    corpus.record("second")
    assert corpus.actions[1]["started_at"] == first["ended_at"]  # type: ignore[index]


def stopped_corpus(tmp_path: Path) -> tuple[Corpus, Mock, Path]:
    """A corpus over a fake server whose stop leaves only `index` and `index-wal`."""
    corpus = Corpus(pins()["fastapi"], tmp_path / "rift", tmp_path / "out" / "r.json")
    root = tmp_path / "workspace"
    (root / ".rift").mkdir(parents=True)
    (root / ".rift" / "index").write_bytes(b"i" * 7)
    (root / ".rift" / "index-wal").write_bytes(b"")
    (root / ".rift" / "metrics").write_bytes(b"m" * 3)
    server = Mock(spec=Server)
    server.root = root
    server.log_path = tmp_path / "out" / "r.server-1.log"
    server.records_path = tmp_path / "out" / "r.server-1.records.log"
    server.started_at = "2026-10-05T09:00:00.000+00:00"
    server.read_records.return_value = (
        "2026-10-05 08:00:00.000Z INFO  rift_index::database::close   "
        "component=storage operation=database.close busy=0 checkpointed=1 "
        "database=earlier log=1  database checkpointed its write-ahead log\n"
    )
    return corpus, server, root


def test_a_stop_keeps_records_and_sizes_with_an_absent_vectors_file(
    tmp_path: Path,
) -> None:
    corpus, server, _ = stopped_corpus(tmp_path)
    corpus.stop(server)
    server.stop.assert_called_once_with()
    server.read_records.assert_called_once_with()
    assert corpus.stops == [
        {
            "stderr": str(server.log_path),
            "sizes": {
                "index": 7,
                "index-wal": 0,
                "metrics": 3,
                "metrics-wal": "absent",
                "vectors": "absent",
                "vectors-wal": "absent",
            },
            "records": str(server.records_path),
            "records_lines": 0,
            "database_close": [],
            "stop_stages": [],
            "lacks": ["database.close", "stop stage ended"],
        }
    ]


def test_a_failing_records_read_is_noted_and_the_case_continues(
    tmp_path: Path,
) -> None:
    corpus, server, _ = stopped_corpus(tmp_path)
    server.read_records.side_effect = RuntimeError("rift exited 3")
    corpus.stop(server)
    assert corpus.stops[0]["records"] is None
    assert corpus.stops[0]["records_error"] == "rift exited 3"
    assert corpus.stops[0]["lacks"] == ["database.close", "stop stage ended"]


STOP_RECORDS = (
    "2026-10-05 09:00:05.100Z INFO  rift_mcp::http::stop_stage   component=mcp "
    "operation=server.stop stage=SQLite worker shutdown  ✓ stop stage ended outcome=ok "
    "remaining=4.9s stage=SQLite worker shutdown\n"
    "2026-10-05 09:00:05.200Z INFO  rift_mcp::http::stop_stage   component=mcp "
    "operation=server.stop stage=SQLite worker shutdown  "
    "database checkpointed its write-ahead log component=storage "
    "operation=database.close busy=0 checkpointed=12 database=index log=12\n"
)


def test_a_stop_reports_database_close_and_stop_stage_values(
    tmp_path: Path,
) -> None:
    corpus, server, _ = stopped_corpus(tmp_path)
    server.read_records.return_value += STOP_RECORDS
    corpus.stop(server)
    entry = corpus.stops[0]
    assert entry["database_close"] == [
        {"database": "index", "busy": 0, "log": 12, "checkpointed": 12}
    ]
    assert entry["stop_stages"] == [
        {"stage": "SQLite worker shutdown", "remaining": "4.9s", "outcome": "ok"}
    ]
    assert entry["lacks"] == []
    assert entry["records_lines"] == 2


def test_a_stop_without_stage_records_names_what_it_lacks(tmp_path: Path) -> None:
    corpus, server, _ = stopped_corpus(tmp_path)
    server.read_records.return_value = STOP_RECORDS.splitlines(keepends=True)[1]
    corpus.stop(server)
    assert corpus.stops[0]["lacks"] == ["stop stage ended"]
    assert corpus.stops[0]["stop_stages"] == []


def test_a_failing_stop_keeps_no_stop_entry(tmp_path: Path) -> None:
    corpus, server, _ = stopped_corpus(tmp_path)
    server.stop.side_effect = AssertionError("server stop exceeded its deadline")
    with pytest.raises(AssertionError, match="exceeded its deadline"):
        corpus.stop(server)
    server.read_records.assert_not_called()
    assert corpus.stops == []


def test_the_report_carries_the_stops(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    corpus, report, _ = corpus_with_server(tmp_path, monkeypatch, fails=False)
    corpus.stops.append({"stderr": "x", "sizes": {"vectors": "absent"}})
    asyncio.run(corpus.run())
    assert json.loads(report.read_text())["stops"] == corpus.stops


def test_the_report_names_the_machine_and_the_run_prints_it_once(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
) -> None:
    corpus, report, _ = corpus_with_server(tmp_path, monkeypatch, fails=False)
    asyncio.run(corpus.run())
    facts = json.loads(report.read_text())["machine"]
    assert isinstance(facts["logical_cpus"], int)
    assert isinstance(facts["system"], str)
    assert isinstance(facts["architecture"], str)
    for key in ("cpu_model", "memory_bytes", "runner_os", "image_version"):
        assert key in facts
    assert facts["memory_bytes"] is None or facts["memory_bytes"] > 0
    lines = [
        line
        for line in capfd.readouterr().out.splitlines()
        if line.startswith("machine:")
    ]
    assert lines == [machine_line(facts)]
