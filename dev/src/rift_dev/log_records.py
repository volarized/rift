"""Read the lines `rift server logs` and server stderr print.

A line is `<timestamp> <LEVEL> <component> <operation> <message>` followed by ` key=value`
pairs, sorted by key, with string values printed without quotes (`LogRecord::rendered`
in `crates/rift-tracing/src/render.rs`). A label a record did not carry prints as `-`.
A record emitted or closed inside a span prints, before its message, the outermost span's
`key=value` fields, then `↳`, the nearest span's name and fields, and two spaces. Stderr
prints the same line with the time in UTC. The runners parse two kinds of record from a
stopped server's output, `database.close` and `stop stage ended`, and pick the newest
`operations in flight` and `metric snapshot` records for a failure window.
"""

from __future__ import annotations

import re
from collections.abc import Iterable, Iterator
from datetime import datetime
from typing import NamedTuple, TypedDict

CHECKPOINTED = "database checkpointed its write-ahead log"
STAGE_ENDED = "stop stage ended"
IN_FLIGHT = "operations in flight"
STALL_REPORT = "operations in flight past the stall delay"
METRIC_SNAPSHOT = "metric snapshot"
DATABASE_CLOSE = "database.close"
# Entries one report keeps per record kind. A stop writes a handful; the newest win.
ENTRIES_MAX = 64
# Characters of a free-text value the report keeps.
VALUE_CHARS_MAX = 300
FIELD_MARK = re.compile(r"(?:^|\s)([A-Za-z_][\w.]*)=")
# The start of the root and nearest span fields a line prints before its message.
SPAN_FIELDS_START = re.compile(r"^(?:[A-Za-z_][\w.]*=|↳ )")
# What ends the root and nearest span fields: the message starts after it.
SPAN_FIELDS_END = "  "


Entry = dict[str, int | str | None]


class Measurements(TypedDict):
    """What `stop_measurements` reports for one stopped server."""

    records_lines: int
    database_close: list[Entry]
    stop_stages: list[Entry]
    lacks: list[str]


class Line(NamedTuple):
    """One printed record, its labels split from its message and fields.

    `spans` holds the root and nearest span fields printed before the message, empty for
    a record outside every span; `rest` holds the message and the record's own fields.
    """

    time: datetime
    level: str
    component: str
    operation: str
    rest: str
    text: str
    spans: str = ""

    def is_message(self, message: str) -> bool:
        """Whether the message is exactly `message`, with fields or nothing after it."""
        return self.rest == message or self.rest.startswith(message + " ")

    def fields(self, message: str) -> dict[str, str]:
        """The `key=value` pairs after `message`; later duplicates win."""
        return fields(self.rest.removeprefix(message))


def parse_line(line: str) -> Line | None:
    """The record a printed line holds, or None for a line of another shape.

    A line that does not start with an ISO 8601 timestamp is not a record, such as
    the cut notice a records file starts with.
    """
    parts = line.split(None, 4)
    if len(parts) < 4:
        return None
    try:
        time = datetime.fromisoformat(parts[0])
    except ValueError:
        return None
    if time.tzinfo is None:
        return None
    spans, rest = split_spans(parts[4] if len(parts) == 5 else "")
    return Line(time, parts[1], parts[2], parts[3], rest, line, spans)


def split_spans(text: str) -> tuple[str, str]:
    """The root and nearest span fields at the start of `text`, and what follows them.

    Span fields print as `key=value` pairs or `↳ name`, and two spaces end them; text
    that starts with neither is the message alone.
    """
    if SPAN_FIELDS_START.match(text) is None:
        return "", text
    spans, separator, rest = text.partition(SPAN_FIELDS_END)
    if not separator:
        return "", text
    return spans, rest.lstrip(" ")


def closes_span(record: Line, name: str) -> bool:
    """Whether `record` is the close record of the span `name`."""
    return record.is_message(name) and record.fields(name).get("span") == "closed"


def instant(text: str) -> datetime | None:
    """The instant an ISO 8601 UTC time names, or None for empty or unreadable text."""
    try:
        return datetime.fromisoformat(text)
    except ValueError:
        return None


def lines(text: str) -> Iterator[Line]:
    """Every record in `text`, oldest first as printed."""
    for line in text.splitlines():
        parsed = parse_line(line)
        if parsed is not None:
            yield parsed


def fields(text: str) -> dict[str, str]:
    """The `key=value` pairs in `text`.

    A value runs to the next ` key=` mark, so a value holding spaces, such as the
    stage `SQLite worker shutdown`, stays whole.
    """
    marks = list(FIELD_MARK.finditer(text))
    found: dict[str, str] = {}
    for index, mark in enumerate(marks):
        end = marks[index + 1].start() if index + 1 < len(marks) else len(text)
        found[mark.group(1)] = text[mark.end() : end].strip()
    return found


def number_or_text(value: str | None) -> int | str | None:
    """An integer value as an int, any other value as bounded text, absence as None."""
    if value is None:
        return None
    if value.lstrip("-").isdigit():
        return int(value)
    return value[:VALUE_CHARS_MAX]


def database_closes(records: Iterable[Line]) -> list[Entry]:
    """The `database.close` records, oldest first, at most `ENTRIES_MAX` newest.

    A record with `busy`, `log`, and `checkpointed` carries the checkpoint's row. A
    close that failed or outlasted its deadline has none, so its entry names the
    database and the message instead.
    """
    found: list[Entry] = []
    for record in records:
        if record.operation != DATABASE_CLOSE:
            continue
        if record.is_message(CHECKPOINTED):
            values = record.fields(CHECKPOINTED)
            found.append(
                {
                    "database": number_or_text(values.get("database")),
                    "busy": number_or_text(values.get("busy")),
                    "log": number_or_text(values.get("log")),
                    "checkpointed": number_or_text(values.get("checkpointed")),
                }
            )
        else:
            values = fields(record.rest)
            found.append(
                {
                    "database": number_or_text(values.get("database")),
                    "busy": None,
                    "log": None,
                    "checkpointed": None,
                    "unchecked": record.rest[:VALUE_CHARS_MAX],
                }
            )
    return found[-ENTRIES_MAX:]


def stop_stages(records: Iterable[Line]) -> list[Entry]:
    """The `stop stage ended` records, oldest first, at most `ENTRIES_MAX` newest.

    `remaining` is the part of the stop's deadline the stage left, as the server
    printed it. A stage that failed also names its `error`.
    """
    found: list[Entry] = []
    for record in records:
        if not record.is_message(STAGE_ENDED):
            continue
        values = record.fields(STAGE_ENDED)
        entry: Entry = {
            "stage": number_or_text(values.get("stage")),
            "remaining": number_or_text(values.get("remaining")),
            "outcome": number_or_text(values.get("outcome")),
        }
        if "error" in values:
            entry["error"] = number_or_text(values["error"])
        found.append(entry)
    return found[-ENTRIES_MAX:]


def stop_measurements(text: str, since: datetime | None = None) -> Measurements:
    """The `database.close` and `stop stage ended` values in one stopped server's records.

    `since` leaves out the records of an earlier server on the same workspace. A
    record kind the text lacks is named in `lacks`, never omitted. `records_lines`
    counts the records read, so a count equal to the read's tail bound shows that
    older records may be missing.
    """
    parsed = [record for record in lines(text) if since is None or record.time >= since]
    closes = database_closes(parsed)
    stages = stop_stages(parsed)
    lacks = []
    if not closes:
        lacks.append(DATABASE_CLOSE)
    if not stages:
        lacks.append(STAGE_ENDED)
    return Measurements(
        records_lines=len(parsed),
        database_close=closes,
        stop_stages=stages,
        lacks=lacks,
    )


def newest_in_flight(records: Iterable[Line]) -> Line | None:
    """The newest `operations in flight` record; a stall report is another message."""
    newest = None
    for record in records:
        if record.is_message(IN_FLIGHT) and not record.is_message(STALL_REPORT):
            newest = record
    return newest


def newest_snapshots(records: Iterable[Line]) -> list[Line]:
    """The newest `metric snapshot` record of each group, oldest first.

    A snapshot record names its group, one of operations, locks, database, runtime,
    process, or lifecycle, in the operation column.
    """
    newest: dict[str, Line] = {}
    for record in records:
        if record.is_message(METRIC_SNAPSHOT):
            newest[record.operation] = record
    return sorted(newest.values(), key=lambda record: record.time)
