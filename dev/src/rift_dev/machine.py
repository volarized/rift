"""Facts about the machine a runner executed on, read with the standard library only."""

from __future__ import annotations

import os
import platform
import subprocess
from pathlib import Path

CPUINFO = Path("/proc/cpuinfo")
CPUINFO_BYTES_MAX = 64 * 1024
SUBPROCESS_SECONDS = 5.0
RUNNER_VARIABLES = {
    "runner_os": "RUNNER_OS",
    "runner_arch": "RUNNER_ARCH",
    "image_os": "ImageOS",
    "image_version": "ImageVersion",
}


def cpu_model() -> str | None:
    """The CPU model text of this host; None when its source is absent or unreadable."""
    try:
        system = platform.system()
        if system == "Linux":
            with CPUINFO.open("rb") as source:
                text = source.read(CPUINFO_BYTES_MAX).decode("utf-8", "replace")
            for line in text.splitlines():
                key, _, value = line.partition(":")
                if key.strip() == "model name" and value.strip():
                    return value.strip()
            return None
        if system == "Darwin":
            done = subprocess.run(
                ["sysctl", "-n", "machdep.cpu.brand_string"],
                capture_output=True,
                text=True,
                timeout=SUBPROCESS_SECONDS,
                check=True,
            )
            return done.stdout.strip() or None
        if system == "Windows":
            return platform.processor() or None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    return None


def memory_bytes() -> int | None:
    """Total physical memory from `os.sysconf` on Unix; None where it has no names."""
    try:
        return os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")
    except (AttributeError, ValueError, OSError):
        return None


def machine() -> dict[str, str | int | None]:
    """The `machine` section of a run document."""
    return {
        "logical_cpus": os.cpu_count(),
        "cpu_model": cpu_model(),
        "memory_bytes": memory_bytes(),
        "system": platform.system() or None,
        "architecture": platform.machine() or None,
        **{key: os.environ.get(name) for key, name in RUNNER_VARIABLES.items()},
    }


def machine_line(facts: dict[str, str | int | None]) -> str:
    """One line carrying every fact, for the job log."""
    return "machine: " + " ".join(f"{key}={facts[key]}" for key in facts)
