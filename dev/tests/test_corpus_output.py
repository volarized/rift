"""Corpus commands expose tested program diagnostics and retain harness evidence."""

from __future__ import annotations

import asyncio
import io
import json
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
from rift_dev.rift_test_client import LOG_BYTES_MAX, Client, Server, stderr_log


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
    assert output.out == output.err == ""
    retained = json.loads(report.read_text())
    assert retained["actions"][0]["symbols"] == 200
    assert retained["status"] == ("failed" if fails else "passed")
    if fails:
        assert "Traceback" in retained["failure"]
        assert "AssertionError: source revision differs" in retained["failure"]
    else:
        assert retained["failure"] == ""


def test_corpus_foreground_stdout_and_stderr_reach_test_output(
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
    assert capfdbinary.readouterr().err == retained


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
    assert path.read_bytes() == output.getvalue()


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
    assert server.log_path.read_bytes() == b"x" * maximum
    assert output.getvalue() == server.log_path.read_bytes()


def test_corpus_connection_forwards_proxy_diagnostics(
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
    assert capfdbinary.readouterr().err == retained


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
