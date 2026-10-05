"""Lines of `rift server logs` parse into the values the runners report."""

from __future__ import annotations

from datetime import UTC, datetime

from rift_dev.log_records import (
    ENTRIES_MAX,
    closes_span,
    instant,
    lines,
    newest_in_flight,
    newest_snapshots,
    parse_line,
    stop_measurements,
)

# Lines a foreground server printed on stderr, as `rift server logs` prints them too: the
# function, the root span's fields, `↳` and the nearest span, then the message.
CHECKPOINT = (
    "2026-10-05 11:57:50.507Z INFO  rift_mcp::http::stop_stage             "
    "component=mcp operation=server.stop stage=SQLite worker shutdown  database "
    "checkpointed its write-ahead log component=storage operation=database.close "
    "busy=0 checkpointed=12 database=index elapsed_ms=1 log=12"
)
STAGE = (
    "2026-10-05 11:57:50.505Z INFO  rift_mcp::http::stop_stage             "
    "component=mcp operation=server.stop stage=SQLite worker shutdown  ✓ stop stage "
    "ended outcome=ok remaining=4.9s stage=SQLite worker shutdown"
)
FAILED_STAGE = (
    "2026-10-05 11:57:50.506Z WARN  rift_mcp::http::stop_stage             "
    "component=mcp operation=server.stop stage=metrics database close  ✗ stop stage "
    "ended causes=timed out: late error=late outcome=error remaining=0ns "
    "stage=metrics database close"
)
IN_FLIGHT = (
    "2026-10-05 11:57:50.504Z INFO  rift_tracing::flight   in_flight=1 left_out=0 "
    'operations=[{"operation":"index.build"}] reason=stop untracked=0  '
    "operations in flight"
)
STALL = (
    "2026-10-05 11:57:50.600Z WARN  rift_tracing::flight   in_flight=1 "
    "reason=stall_delay  operations in flight past the stall delay"
)
NESTED = (
    "2026-10-05 11:57:37.429Z DEBUG rift_mcp::validation::run_index_supervisor_with   "
    "component=index epoch=1 trigger=filesystem        ↳ worker.run               "
    "component=worker operation=worker.run work=filesystem index rebuild → index capture "
    "started component=index operation=index.build epoch=1 phase=start"
)
CLOSE = (
    "2026-10-05 11:57:37.444Z INFO  rift_mcp::validation::run_index_supervisor_with   "
    "component=index epoch=1 trigger=filesystem  ↳ index.build component=index "
    "changed_count=1 outcome=ok close ✓ busy=7.95ms idle=19.3µs"
)
ROOT_CLOSE = (
    "2026-10-05 11:57:15.826Z INFO  rift_mcp::history::HistoryTask::fill_planned   "
    "component=history operation=history.batch  close ✓ busy=65.3µs idle=7.08µs"
)
TRAIT_METHOD = (
    "2026-10-05 11:57:41.276Z INFO  <rift_mcp::server::RiftMcp as "
    "rmcp::handler::server::ServerHandler>::call_tool   component=mcp "
    "operation=tools/call req=1 tool=get_symbol  close ✓ busy=1.23ms idle=1.70ms"
)


def test_a_line_splits_into_its_columns() -> None:
    record = parse_line(CHECKPOINT)
    assert record is not None
    assert record.level == "INFO"
    assert record.function == "rift_mcp::http::stop_stage"
    assert (record.component, record.operation) == ("storage", "database.close")
    assert record.time == datetime(2026, 10, 5, 11, 57, 50, 507000, tzinfo=UTC)
    assert record.is_message("database checkpointed its write-ahead log")
    assert record.context == (
        "component=mcp operation=server.stop stage=SQLite worker shutdown"
    )


def test_the_span_fields_and_the_nearest_span_stay_out_of_the_message() -> None:
    record = parse_line(NESTED)
    assert record is not None
    assert (record.level, record.component, record.operation) == (
        "DEBUG",
        "index",
        "index.build",
    )
    assert record.context == "component=index epoch=1 trigger=filesystem"
    assert record.nested_name == "worker.run"
    assert record.is_message("index capture started")
    assert record.fields("index capture started")["phase"] == "start"
    assert record.fields("index capture started")["epoch"] == "1"


def test_a_span_close_names_the_span_after_the_mark_or_by_its_operation() -> None:
    nested = parse_line(CLOSE)
    root = parse_line(ROOT_CLOSE)
    assert nested is not None
    assert root is not None
    assert closes_span(nested, "index.build")
    assert not closes_span(nested, "index.publish")
    assert closes_span(root, "history.batch")
    started = parse_line(NESTED)
    assert started is not None
    assert not closes_span(started, "index.build")


def test_a_failed_close_is_a_close_of_its_span() -> None:
    for message in (
        "close ✗ busy=7.95ms idle=19.3µs",
        "close ✗ error.type=timeout busy=7.95ms idle=19.3µs",
        "close ✗ panicked busy=7.95ms idle=19.3µs",
        "close ✗ cancelled busy=7.95ms idle=19.3µs",
    ):
        failed = parse_line(
            CLOSE.replace(
                "outcome=ok close ✓ busy=7.95ms idle=19.3µs", f"outcome=error {message}"
            )
        )
        assert failed is not None
        assert failed.label("outcome") == "error"
        assert failed.closes(), message
        assert closes_span(failed, "index.build"), message
        assert failed.nested_name == "index.build"


def test_a_trait_method_function_keeps_its_spaces() -> None:
    record = parse_line(TRAIT_METHOD)
    assert record is not None
    assert record.function == (
        "<rift_mcp::server::RiftMcp as rmcp::handler::server::ServerHandler>::call_tool"
    )
    assert record.operation == "tools/call"
    assert record.closes()


def test_an_escaped_control_character_stays_on_its_line() -> None:
    escaped = (
        "2026-10-05 11:57:41.276Z WARN  rift_mcp::server::read   component=mcp "
        "path=a\\u{1b}[2Jb\\nc  read refused"
    )
    assert [record.is_message("read refused") for record in lines(escaped)] == [True]
    record = parse_line(escaped)
    assert record is not None
    assert record.fields("read refused")["path"] == "a\\u{1b}[2Jb\\nc"


def test_a_line_of_another_shape_is_not_a_record() -> None:
    cut = "[12 earlier bytes were left out]"
    assert parse_line(cut) is None
    assert parse_line("") is None
    assert parse_line("not-a-time INFO a b message") is None
    assert parse_line("2026-10-05T10:27:23.437+00:00 INFO  mcp  -  late") is None
    assert [record.level for record in lines(f"{cut}\n{STAGE}\n\n")] == ["INFO"]


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
        "2026-10-05 09:00:05.000Z WARN  rift_index::database::close   component=storage "
        "operation=database.close database=vectors  database checkpoint outlasted the "
        "shutdown deadline; the write-ahead log stays for the next open"
    )
    entry = stop_measurements(skipped)["database_close"][0]
    assert entry["database"] == "vectors"
    assert entry["busy"] is None
    assert str(entry["unchecked"]).startswith("database checkpoint outlasted")


def test_records_of_an_earlier_server_are_left_out() -> None:
    since = datetime(2026, 10, 5, 11, 57, 50, 506000, tzinfo=UTC)
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
        return f"2026-10-05 09:00:0{second}.000Z INFO  {group}   a=1"

    kept = newest_snapshots(
        lines(
            "\n".join(
                [snapshot(1, "locks"), snapshot(2, "database"), snapshot(3, "locks")]
            )
        )
    )
    assert [record.operation for record in kept] == ["database", "locks"]
    assert kept[1].time.second == 3
