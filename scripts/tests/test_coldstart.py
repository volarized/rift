"""Container ownership failures preserve the original gate error and attempt cleanup."""

from __future__ import annotations

import asyncio
from collections.abc import Mapping, Sequence
from pathlib import Path

import check_coldstart
import pytest


def test_failed_container_start_still_attempts_cleanup(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    binary = tmp_path / "rift"
    binary.write_bytes(b"\x7fELFbinary")
    commands: list[list[str]] = []

    def failed_run(
        command: Sequence[str],
        *,
        cwd: Path | None = None,
        env: Mapping[str, str] | None = None,
        timeout_seconds: float = 30.0,
    ) -> str:
        commands.append(list(command))
        raise RuntimeError(f"failed {command[1]}")

    def failed_cleanup(command: Sequence[str], *, timeout: float) -> str:
        return failed_run(command, timeout_seconds=timeout)

    monkeypatch.setattr(check_coldstart, "run_command", failed_run)
    monkeypatch.setattr(check_coldstart, "run", failed_cleanup)
    with pytest.raises(RuntimeError, match="failed run") as captured:
        asyncio.run(check_coldstart.check_coldstart(binary, "ubuntu:24.04"))
    assert [command[1] for command in commands] == ["run", "logs", "rm"]
    assert len(captured.value.__notes__) == 2
    command = commands[0]
    assert command[command.index("--network") + 1] == "none"
    assert command[command.index("--workdir") + 1] == "/workspace"
    mount = command[command.index("--mount") + 1]
    assert mount.endswith("target=/rift,readonly")


def test_expired_gate_still_removes_container(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from types import SimpleNamespace
    from unittest.mock import Mock

    import release_process
    import rift_test_client

    binary = tmp_path / "rift"
    binary.write_bytes(b"\x7fELFbinary")
    clock = [100.0]
    fake_time = SimpleNamespace(monotonic=lambda: clock[0])
    monkeypatch.setattr(rift_test_client, "time", fake_time)
    monkeypatch.setattr(release_process, "time", fake_time)
    monkeypatch.setattr(check_coldstart, "COLDSTART_SECONDS", 1.0)
    process = Mock(side_effect=AssertionError("expired deadline started a process"))
    monkeypatch.setattr(release_process, "owned_process", process)
    cleanup: list[tuple[str, float]] = []

    def expire(command: Sequence[str], *, timeout_seconds: float) -> str:
        clock[0] += 2.0
        return rift_test_client.run_command(command, timeout_seconds=timeout_seconds)

    def collect(command: Sequence[str], *, timeout: float) -> str:
        assert rift_test_client.remaining_seconds(1.0) == 0.0
        cleanup.append((command[1], timeout))
        return ""

    monkeypatch.setattr(check_coldstart, "run_command", expire)
    monkeypatch.setattr(check_coldstart, "run", collect)
    with pytest.raises(RuntimeError, match="command deadline expired"):
        asyncio.run(check_coldstart.check_coldstart(binary, "ubuntu:24.04"))
    assert cleanup == [("logs", 10), ("rm", 30)]
    process.assert_not_called()
    assert rift_test_client.remaining_seconds(30.0) == 30.0
