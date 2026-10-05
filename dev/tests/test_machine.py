"""The machine facts degrade to None and never raise."""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest
from rift_dev import machine


def test_unreadable_linux_cpu_source_gives_none(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(machine.platform, "system", lambda: "Linux")
    monkeypatch.setattr(machine, "CPUINFO", tmp_path / "absent")
    assert machine.cpu_model() is None


def test_linux_cpu_model_is_the_first_model_name_line(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source = tmp_path / "cpuinfo"
    source.write_text("processor : 0\nmodel name\t: AMD EPYC\nmodel name\t: other\n")
    monkeypatch.setattr(machine.platform, "system", lambda: "Linux")
    monkeypatch.setattr(machine, "CPUINFO", source)
    assert machine.cpu_model() == "AMD EPYC"


def test_linux_cpu_source_is_read_to_the_byte_bound(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    source = tmp_path / "cpuinfo"
    source.write_text("x" * machine.CPUINFO_BYTES_MAX + "\nmodel name : late\n")
    monkeypatch.setattr(machine.platform, "system", lambda: "Linux")
    monkeypatch.setattr(machine, "CPUINFO", source)
    assert machine.cpu_model() is None


def test_failed_or_slow_sysctl_gives_none(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(machine.platform, "system", lambda: "Darwin")

    def slow(*_args: object, **_kwargs: object) -> None:
        raise subprocess.TimeoutExpired("sysctl", machine.SUBPROCESS_SECONDS)

    monkeypatch.setattr(machine.subprocess, "run", slow)
    assert machine.cpu_model() is None


def test_unnamed_sysconf_gives_none_memory(monkeypatch: pytest.MonkeyPatch) -> None:
    def refuse(_name: str) -> int:
        raise ValueError("unrecognized configuration name")

    monkeypatch.setattr(machine.os, "sysconf", refuse)
    assert machine.memory_bytes() is None


def test_runner_facts_read_their_environment_names(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    for name in machine.RUNNER_VARIABLES.values():
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("ImageOS", "ubuntu24")
    facts = machine.machine()
    assert facts["image_os"] == "ubuntu24"
    assert facts["runner_os"] is None
