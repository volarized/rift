"""Lines of `rift server logs` parse into the values the runners report."""

from __future__ import annotations

from datetime import UTC, datetime

from rift_dev.log_records import (
    ENTRIES_MAX,
    instant,
    lines,
    newest_in_flight,
    newest_snapshots,
    parse_line,
    stop_measurements,
)

CHECKPOINT = (
    "2026-10-05T09:00:05.200+00:00 🔵 INFO  storage  database.close "
    "database checkpointed its write-ahead log "
    "busy=0 checkpointed=12 database=index log=12"
)
STAGE = (
    "2026-10-05T09:00:05.100+00:00 🔵 INFO  mcp      server.stop  stop stage ended "
    "outcome=ok remaining=4.9s stage=SQLite worker shutdown"
)
FAILED_STAGE = (
    "2026-10-05T09:00:05.300+00:00 🟡 WARN  mcp      server.stop  stop stage ended "
    "causes=timed out: late error=late outcome=error remaining=0ns "
    "stage=metrics database close"
)
IN_FLIGHT = (
    "2026-10-05T09:00:04.000+00:00 🔵 INFO  -        -            operations in flight "
    'in_flight=1 left_out=0 operations=[{"operation":"index.build"}] '
    "reason=stop untracked=0"
)
STALL = (
    "2026-10-05T09:00:04.500+00:00 🟡 WARN  -        -            "
    "operations in flight past the stall delay in_flight=1 reason=stall_delay"
)


def test_a_line_splits_into_its_labels_and_message() -> None:
    record = parse_line(CHECKPOINT)
    assert record is not None
    assert (record.level, record.component, record.operation) == (
        "INFO",
        "storage",
        "database.close",
    )
    assert record.time == datetime(2026, 10, 5, 9, 0, 5, 200000, tzinfo=UTC)
    assert record.is_message("database checkpointed its write-ahead log")


def test_a_line_of_another_shape_is_not_a_record() -> None:
    cut = "[12 earlier bytes were left out]"
    assert parse_line(cut) is None
    assert parse_line("") is None
    assert parse_line("not-a-time 🔵 INFO a b message") is None
    assert [record.level for record in lines(f"{cut}\n{STAGE}\n")] == ["INFO"]


def test_an_instant_reads_the_runners_timestamps() -> None:
    assert instant("2026-10-05T09:00:00.000+00:00") == datetime(
        2026, 10, 5, 9, tzinfo=UTC
    )
    assert instant("") is None


def test_a_stop_reports_close_values_and_stage_values() -> None:
    found = stop_measurements(f"{STAGE}\n{CHECKPOINT}\n{FAILED_STAGE}" + "\n")
    assert found["database_close"] == [
        {"database": "index", "busy": 0, "log": 12, "checkpointed": 12}
    ]
    assert found["stop_stages"] == [
        {"stage": "SQLite worker shutdown", "remaining": "4.9s", "outcome": "ok"},
        {
            "stage": "metrics database close",
            "remaining": "0ns",
            "outcome": "error",
            "error": "late",
        },
    ]
    assert found["lacks"] == []
    assert found["records_lines"] == 3


def test_a_missing_record_kind_is_named_never_omitted() -> None:
    assert stop_measurements(STAGE)["lacks"] == ["database.close"]
    assert stop_measurements(CHECKPOINT)["lacks"] == ["stop stage ended"]
    empty = stop_measurements("")
    assert empty["lacks"] == ["database.close", "stop stage ended"]
    assert empty["database_close"] == []
    assert empty["stop_stages"] == []


def test_a_close_without_a_checkpoint_row_keeps_its_message() -> None:
    skipped = (
        "2026-10-05T09:00:05.000+00:00 🟡 WARN  storage  database.close "
        "database checkpoint outlasted the shutdown deadline; the write-ahead log "
        "stays for the next open database=vectors"
    )
    entry = stop_measurements(skipped)["database_close"][0]
    assert entry["database"] == "vectors"
    assert entry["busy"] is None
    assert str(entry["unchecked"]).startswith("database checkpoint outlasted")


def test_records_of_an_earlier_server_are_left_out() -> None:
    since = datetime(2026, 10, 5, 9, 0, 5, 150000, tzinfo=UTC)
    found = stop_measurements(f"{STAGE}\n{CHECKPOINT}", since)
    assert found["stop_stages"] == []
    assert found["lacks"] == ["stop stage ended"]
    assert found["records_lines"] == 1


def test_the_report_keeps_the_newest_entries_within_its_bound() -> None:
    text = "\n".join(
        STAGE.replace("stage=SQLite", f"stage=s{n} SQLite")
        for n in range(ENTRIES_MAX + 5)
    )
    stages = stop_measurements(text)["stop_stages"]
    assert len(stages) == ENTRIES_MAX
    assert stages[-1]["stage"] == f"s{ENTRIES_MAX + 4} SQLite worker shutdown"


def test_the_newest_operations_in_flight_is_not_a_stall_report() -> None:
    found = newest_in_flight(lines(f"{IN_FLIGHT}\n{STALL}"))
    assert found is not None
    assert found.text == IN_FLIGHT
    assert newest_in_flight(lines(STALL)) is None


def test_the_newest_snapshot_of_each_group_is_kept() -> None:
    def snapshot(second: int, group: str) -> str:
        return (
            f"2026-10-05T09:00:0{second}.000+00:00 🔵 INFO  -        {group:<12} "
            "metric snapshot a=1"
        )

    kept = newest_snapshots(
        lines(
            "\n".join(
                [snapshot(1, "locks"), snapshot(2, "database"), snapshot(3, "locks")]
            )
        )
    )
    assert [record.operation for record in kept] == ["database", "locks"]
    assert kept[1].time.second == 3
