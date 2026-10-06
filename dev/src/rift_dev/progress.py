"""Print one timestamped start and finish line for a development run."""

from __future__ import annotations

import time
from datetime import datetime
from typing import Literal

RunLabel = Literal["build", "check", "clippy", "docs", "tests"]
_ICONS = {
    "build": "🔨",
    "check": "🔎",
    "clippy": "🧹",
    "docs": "📚",
    "tests": "🧪",
}


def start(label: RunLabel) -> float:
    """Print the start time and return monotonic time for `finish`."""
    print(f"{_ICONS[label]} {label} started at {_timestamp()}", flush=True)
    return time.monotonic()


def finish(label: RunLabel, started: float, *, failed: bool) -> None:
    """Print the finish time, elapsed time, and result of a run."""
    elapsed = max(0.0, time.monotonic() - started)
    icon = "❌" if failed else "✅"
    result = "failures detected" if failed else (", all green" if label == "tests" else "")
    print(
        f"{icon} {label} finished at {_timestamp()}, total time: {elapsed:.1f}s{result}",
        flush=True,
    )


def _timestamp() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")
