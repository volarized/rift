"""Container ownership failures preserve the original gate error and attempt cleanup."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path

import pytest
from rift_dev import check_coldstart
from rift_dev.commands import Command, DockerCommand
from rift_dev.rift_test_client import LOG_BYTES_MAX, LOG_FILTER


def test_failed_container_start_still_attempts_cleanup(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    binary = tmp_path / "rift"
    binary.write_bytes(b"\x7fELFbinary")
    commands: list[list[str]] = []

    def failed_output(command: Command) -> str:
        commands.append(command.argv)
        raise RuntimeError(f"failed {command.arguments[0]}")

    monkeypatch.setattr(DockerCommand, "output", failed_output)
    with pytest.raises(RuntimeError, match="failed run") as captured:
        asyncio.run(check_coldstart.check_coldstart(binary, "ubuntu:24.04"))
    assert [command[1] for command in commands] == [
        "run",
        "logs",
        "exec",
        "exec",
        "exec",
        "rm",
    ]
    notes = captured.value.__notes__
    assert [note.split(":")[0] for note in notes[:3]] == [
        "server stderr (last 200 lines) unavailable",
        "persisted log records unavailable",
        "rift mcp stderr unavailable",
    ]
    assert notes[3].startswith("failure window: log records from")
    assert notes[3].endswith("unavailable: failed exec")
    assert notes[4] == ("newest operations in flight unavailable: failed exec")
    assert notes[5].startswith("container cleanup failed")
    command = commands[0]
    assert command[command.index("--env") + 1] == f"RUST_LOG={LOG_FILTER}"
    assert command[command.index("--network") + 1] == "none"
    assert command[command.index("--workdir") + 1] == "/workspace"
    mount = command[command.index("--mount") + 1]
    assert mount.endswith("target=/rift,readonly")


def test_expired_gate_still_removes_container(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from types import SimpleNamespace
    from unittest.mock import Mock

    from rift_dev import commands, rift_test_client

    binary = tmp_path / "rift"
    binary.write_bytes(b"\x7fELFbinary")
    clock = [100.0]
    fake_time = SimpleNamespace(monotonic=lambda: clock[0])
    monkeypatch.setattr(rift_test_client, "time", fake_time)
    monkeypatch.setattr(commands, "time", fake_time)
    monkeypatch.setattr(check_coldstart, "COLDSTART_SECONDS", 1.0)
    process = Mock(side_effect=AssertionError("expired deadline started a process"))
    monkeypatch.setattr(commands, "owned_process", process)
    cleanup: list[tuple[str, float | None]] = []
    captured_output = Command.output

    def output(command: Command) -> str:
        if command.arguments[0] == "run":
            clock[0] += 2.0
            return captured_output(command)
        assert rift_test_client.remaining_seconds(1.0) == 0.0
        cleanup.append((command.arguments[0], command.timeout_seconds))
        return ""

    monkeypatch.setattr(DockerCommand, "output", output)
    with pytest.raises(RuntimeError, match="command deadline expired"):
        asyncio.run(check_coldstart.check_coldstart(binary, "ubuntu:24.04"))
    # Evidence reads: stderr, records, then the failure window's two reads.
    assert cleanup == [
        ("logs", 10),
        ("exec", 10),
        ("exec", 10),
        ("exec", 10),
        ("rm", 30),
    ]
    process.assert_not_called()
    assert rift_test_client.remaining_seconds(30.0) == 30.0


@pytest.mark.skipif(os.name == "nt", reason="cold container uses a POSIX shell")
def test_stop_waits_for_delayed_process_exit_and_status(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import shlex
    import sys
    import tempfile
    import threading
    import time

    from rift_dev.commands import owned_process

    request = tmp_path / "stop.request"
    status = tmp_path / "server.exit"
    binary = tmp_path / "rift"
    binary.write_text(f"#!/bin/sh\n: > {shlex.quote(str(request))}\n")
    binary.chmod(0o755)
    child = (
        "import pathlib,sys,time\nrequest=pathlib.Path(sys.argv[1])\n"
        "while not request.exists():\n time.sleep(0.01)\ntime.sleep(0.1)\n"
    )
    with (
        tempfile.TemporaryFile() as stdin,
        owned_process(
            [sys.executable, "-c", child, str(request)], None, tmp_path, stdin
        ) as process,
    ):

        def record_exit() -> None:
            result = process.wait(timeout=5)
            status.touch()
            time.sleep(0.05)
            status.write_text(str(result))

        writer = threading.Thread(target=record_exit, daemon=True)
        writer.start()

        def inside(name: str, arguments: list[str], timeout: float = 30.0) -> str:
            assert name == "fixture"
            assert arguments[4:] == ["/rift", str(process.pid), "/server.exit"]
            assert 0 < timeout <= check_coldstart.STOP_SECONDS
            command = [*arguments[:4], str(binary), str(process.pid), str(status)]
            return Command(*command).with_timeout(timeout).output()

        monkeypatch.setattr(check_coldstart, "container_command", inside)
        try:
            check_coldstart.stop_container("fixture", process.pid)
            assert request.exists()
            assert status.read_text() == "0"
            assert process.poll() == 0
        finally:
            if process.poll() is None:
                process.kill()
            writer.join(timeout=5)
            assert not writer.is_alive()
            assert process.stdout is not None and process.stderr is not None
            process.stdout.close()
            process.stderr.close()


def test_stop_rejects_status_after_original_deadline(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from types import SimpleNamespace

    clock = [100.0]
    monkeypatch.setattr(
        check_coldstart, "time", SimpleNamespace(monotonic=lambda: clock[0])
    )

    def late_status(_name: str, _arguments: list[str], timeout: float = 30.0) -> str:
        assert timeout == check_coldstart.STOP_SECONDS
        clock[0] += check_coldstart.STOP_SECONDS + 1.0
        return "0\n"

    monkeypatch.setattr(check_coldstart, "container_command", late_status)
    with pytest.raises(AssertionError, match="cold server stop exceeded"):
        check_coldstart.stop_container("fixture", 7)


@pytest.mark.parametrize(
    ("failed_launch", "cause_message"),
    [
        (True, "No such file or directory (os error 2)"),
        (True, None),
        (False, None),
    ],
)
def test_missing_engine_requires_launch_failure_and_preserves_syntax_reads(
    monkeypatch: pytest.MonkeyPatch,
    failed_launch: bool,
    cause_message: str | None,
) -> None:
    from contextlib import nullcontext
    from typing import cast
    from unittest.mock import AsyncMock

    from rift_dev.rift_test_client import Client, JsonObject, parse_failure

    client = AsyncMock(spec=Client)
    reads = []

    async def call(name: str, arguments: JsonObject) -> JsonObject:
        if name == "search":
            if failed_launch:
                head = "\tcapability_unavailable · retry operator_action\n"
                text = f"1 error\n{head}\t\tlanguage engine could not start\n"
                if cause_message is not None:
                    text = (
                        f"2 errors\n{head}\t\tlanguage engine could not start\n"
                        f"{head}\t\t{cause_message}\n"
                    )
                raise parse_failure(name, text)
            return {"results": []}
        assert name == "get_symbol"
        reads.append(arguments)
        return {
            "hits": [
                {
                    "symbol": {
                        "name": "beacon_cold",
                        "id": "rift://symbol/rust/lib.rs/beacon_cold",
                    },
                    "source": check_coldstart.SOURCE.rstrip("\n"),
                }
            ]
        }

    client.call.side_effect = call
    client.resource.return_value = {"records": [{"message": "launch failed"}]}
    commands = []
    monkeypatch.setattr(
        check_coldstart,
        "container_command",
        lambda name, arguments: commands.append((name, arguments)),
    )
    if cause_message is not None:
        expected = nullcontext()
    elif failed_launch:
        expected = pytest.raises(AssertionError, match="lost launch cause")
    else:
        expected = pytest.raises(AssertionError, match="answered references")
    with expected:
        asyncio.run(
            check_coldstart.check_missing_executable(cast(Client, client), "fixture")
        )
    assert len(reads) == (2 if cause_message is not None else 1)
    assert commands[0][1][-1] == '[languages.rust.lsp]\ncommand = ["rust-analyzer"]\n'


def test_container_evidence_keeps_server_stderr_proxy_stderr_and_records(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    proxy = tmp_path / "proxy.log"
    proxy.write_text("proxy: connection reset\n", encoding="utf-8")
    answers = {
        "logs": "server: panicked at index.rs\n",
        "exec": "2026-10-05T09:00:00Z ERROR index build failed\n",
    }
    limits: list[tuple[str, float | None, int]] = []

    def output(command: Command) -> str:
        limits.append(
            (command.arguments[0], command.timeout_seconds, command.output_bytes_max)
        )
        return answers[command.arguments[0]]

    monkeypatch.setattr(DockerCommand, "output", output)
    notes = check_coldstart.container_evidence(
        "cold", proxy, "2026-10-05T08:59:00.000+00:00"
    )
    assert notes[:3] == [
        "server stderr (last 200 lines):\nserver: panicked at index.rs\n",
        "persisted log records:\n2026-10-05T09:00:00Z ERROR index build failed\n",
        f"rift mcp stderr ({proxy}):\nproxy: connection reset\n",
    ]
    # The window reads the container's own `rift server logs` twice: the window, then
    # every record before the failure.
    assert notes[3].startswith(
        "failure window: log records from 2026-10-05T08:59:00.000+00:00 "
        "(read just before the container was created) until "
    )
    assert len(notes) == 5
    # Each read has its own bound and no gate deadline: a timed-out gate is what
    # the evidence explains.
    assert limits == [
        ("logs", check_coldstart.EVIDENCE_SECONDS, LOG_BYTES_MAX),
        ("exec", check_coldstart.EVIDENCE_SECONDS, LOG_BYTES_MAX),
        ("exec", check_coldstart.EVIDENCE_SECONDS, LOG_BYTES_MAX),
        ("exec", check_coldstart.EVIDENCE_SECONDS, LOG_BYTES_MAX),
    ]


def test_container_evidence_failure_never_raises(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def broken(command: Command) -> str:
        raise RuntimeError("docker is gone")

    monkeypatch.setattr(DockerCommand, "output", broken)
    notes = check_coldstart.container_evidence(
        "cold", tmp_path / "missing.log", "2026-10-05T08:59:00.000+00:00"
    )
    assert notes[0] == "server stderr (last 200 lines) unavailable: docker is gone"
    assert notes[1] == "persisted log records unavailable: docker is gone"
    assert notes[2].startswith("rift mcp stderr unavailable:")
    assert notes[3].endswith("unavailable: docker is gone")
    assert notes[4] == ("newest operations in flight unavailable: docker is gone")
