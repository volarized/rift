"""Read the lines `rift server logs` and server stderr print.

A line is `<date> <time>Z <LEVEL> <function>`, three spaces, the context, two spaces, the
nested operation, and the message (`LogLines` in `crates/rift-tracing/src/render.rs`):

```text
2026-10-04 20:42:58.798Z INFO  rift_mcp::server::RiftMcp::nodes   component=mcp operation=tools/call req=11 tool=nodes  ↳ fingerprint.fold component=index operation=fingerprint.fold close ✓ busy=12.1µs idle=13.2µs
```

The context holds the root span's `key=value` fields, or, for a record outside every
span, the record's own `component`, `operation`, and fields. The nested operation is `↳`,
the nearest span's name, and its fields. The message is `close`, a mark, and `busy` and
`idle` for a span close; a mark (`→`, `✓`, `✗`) and the message for a lifecycle record;
the message and the record's own fields otherwise. A stored page pads each column to the
widest value of its group and puts a blank line between groups; a live stream pads to
fixed widths. The runners parse two kinds of record from a stopped server's output,
`database.close` and `stop stage ended`, and pick the newest `operations in flight`
record for a failure window.
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
DATABASE_CLOSE = "database.close"
# Entries one report keeps per record kind. A stop writes a handful; the newest win.
ENTRIES_MAX = 64
# Characters of a free-text value the report keeps.
VALUE_CHARS_MAX = 300
FIELD_MARK = re.compile(r"(?:^|\s)([A-Za-z_][\w.]*)=")
# One `key=value` field of the nearest span, its value up to the next space.
NESTED_FIELD = re.compile(r"[A-Za-z_][\w.]*=\S*\s*")
# What ends the context: the nested operation or the message starts after it.
CONTEXT_END = "  "
# The mark before the nearest span of a record nested below the root span.
NESTED_MARK = "↳ "
# The marks a span close or a lifecycle record prints before its message.
MARKS = "→✓✗"
# A span close: `close`, then its mark when the record states one.
CLOSE = re.compile(r"^close(?: [✓✗])?(?:\s|$)")


Entry = dict[str, int | str | None]


class Measurements(TypedDict):
    """What `stop_measurements` reports for one stopped server."""

    records_lines: int
    database_close: list[Entry]
    stop_stages: list[Entry]
    lacks: list[str]


class Line(NamedTuple):
    """One printed record, its columns split.

    `context` and `nested` hold the `key=value` fields of the root and the nearest span,
    `nested_name` the nearest span's name, empty for a record of the root span; `rest`
    holds the message and the record's own fields.
    """

    time: datetime
    level: str
    function: str
    context: str
    nested_name: str
    nested: str
    rest: str
    text: str

    @property
    def component(self) -> str:
        """The record's `component`: its own, else the nearest span's, else the root's."""
        return self.label("component")

    @property
    def operation(self) -> str:
        """The record's `operation`: its own, else the nearest span's, else the root's."""
        return self.label("operation")

    def label(self, key: str) -> str:
        """`key` among the record's own fields, then the nearest span's, then the root's."""
        for text in (self.rest, self.nested, self.context):
            value = fields(text).get(key)
            if value is not None:
                return value
        return ""

    def message_end(self, message: str) -> int | None:
        """Where `message` ends in `rest`, after an optional mark; None when absent.

        The message starts the rest, or follows a value of the nearest span that holds
        spaces, which the line prints without quotes.
        """
        pattern = rf"(?:^|\s)(?:[{MARKS}] )?{re.escape(message)}(?=\s|$)"
        found = re.search(pattern, self.rest)
        return None if found is None else found.end()

    def is_message(self, message: str) -> bool:
        """Whether the record's message is `message`, with fields or nothing after it."""
        return self.message_end(message) is not None

    def fields(self, message: str) -> dict[str, str]:
        """The `key=value` pairs of the root span, the nearest span, and the record after
        `message`; later ones win, so the record's own fields come last."""
        end = self.message_end(message)
        own = self.rest[end:] if end is not None else self.rest
        return {**fields(self.context), **fields(self.nested), **fields(own)}

    def closes(self) -> bool:
        """Whether the record is a span close."""
        return CLOSE.search(self.rest) is not None


def parse_line(line: str) -> Line | None:
    """The record a printed line holds, or None for a line of another shape.

    A line that does not start with a UTC timestamp is not a record, such as the cut
    notice a records file starts with or the blank line between two groups.
    """
    parts = line.split(None, 3)
    if len(parts) < 4 or not parts[1].endswith("Z"):
        return None
    try:
        time = datetime.fromisoformat(f"{parts[0]}T{parts[1][:-1]}+00:00")
    except ValueError:
        return None
    function, after = split_function(parts[3])
    context, nested_name, nested, rest = split_columns(after)
    return Line(time, parts[2], function, context, nested_name, nested, rest, line)


def split_function(text: str) -> tuple[str, str]:
    """The function column at the start of `text`, and what follows it.

    A trait method prints as `<Type as Trait>::method`, which holds spaces.
    """
    if text.startswith("<"):
        close = text.find(">::")
        if close != -1:
            end = text.find(" ", close)
            end = len(text) if end == -1 else end
            return text[:end], text[end:].lstrip(" ")
    function, _, after = text.partition(" ")
    return function, after.lstrip(" ")


def split_columns(text: str) -> tuple[str, str, str, str]:
    """The context, the nearest span's name and fields, and the rest of `text`.

    The context is `key=value` pairs ended by two spaces; without them, `text` is the
    message alone. The nearest span's fields follow its name up to the first word that is
    not `key=value`.
    """
    context = ""
    if FIELD_MARK.match(text) and CONTEXT_END in text:
        context, _, text = text.partition(CONTEXT_END)
        text = text.lstrip(" ")
    if not text.startswith(NESTED_MARK):
        return context.rstrip(), "", "", text
    name, _, after = text[len(NESTED_MARK) :].partition(" ")
    after = after.lstrip(" ")
    position = 0
    while (found := NESTED_FIELD.match(after, position)) is not None:
        position = found.end()
    return context.rstrip(), name, after[:position].rstrip(), after[position:]


def closes_span(record: Line, name: str) -> bool:
    """Whether `record` is the close record of the span `name`, under `✓` or `✗`.

    A nested close names its span after `↳`. A root span prints no name, so its close is
    the close of the span whose `operation` is `name`, as `traced!` names a span after its
    operation.
    """
    if not record.closes():
        return False
    if record.nested_name:
        return record.nested_name == name
    return fields(record.context).get("operation") == name


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
            values = {"database": record.label("database") or None}
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
