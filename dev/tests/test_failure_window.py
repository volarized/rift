"""A failure keeps the records between its lower bound and the failure."""

from __future__ import annotations

import os
from collections.abc import Sequence
from pathlib import Path

import pytest
from rift_dev.rift_test_client import (
    EVIDENCE_TAIL_BYTES,
    RECORD_TAIL,
    LogsReader,
    Server,
    failure_window,
    logs_arguments,
)

SINCE = "2026-10-05T09:00:00.000+00:00"
UNTIL = "2026-10-05T09:00:30.000+00:00"
IN_FLIGHT = (
    "2026-10-05 08:59:00.000Z INFO  rift_tracing::flight   in_flight=2 reason=stop  "
    "operations in flight"
)
SNAPSHOT = "2026-10-05 08:59:30.000Z INFO  locks   a=1"
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
        "--kind",
        "all",
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
    seen, read = reads(INSIDE + "\n", f"{IN_FLIGHT}\n{SNAPSHOT}" + "\n")
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
        f"failure window: records of every kind from {SINCE} (the end of the last "
        f"recorded action, publication) until {UNTIL}, the newest {RECORD_TAIL} at "
        f"most\n{INSIDE}\n"
    )
    assert notes[1] == (
        f"newest operations in flight record before {UNTIL}:\n{IN_FLIGHT}"
    )
    assert (
        notes[2] == f"newest metric snapshot of each group before {UNTIL}:\n{SNAPSHOT}"
    )


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
    assert notes[2].endswith(f"(none among the newest {RECORD_TAIL} records)")


def test_a_failed_read_is_a_note_and_never_raises() -> None:
    def read(arguments: Sequence[str]) -> str:
        raise RuntimeError("rift exited 3")

    notes = failure_window(
        read, since=SINCE, lower_bound="the server's start", until=UNTIL
    )
    assert notes[0].endswith("unavailable: rift exited 3")
    assert notes[1] == (
        "newest operations in flight and metric snapshots unavailable: rift exited 3"
    )
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
        f"failure window: records of every kind from {SINCE} (the server's start)"
    )
    assert "build failed" in server.window_path.read_text()
    assert server.window_path == tmp_path / "server.window.log"
    later = server.window("2026-10-05T09:00:20.000+00:00", "a later bound")
    assert "from 2026-10-05T09:00:20.000+00:00 (a later bound)" in later[0]
