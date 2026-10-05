"""Own real Rift processes and validate messages through the MCP Python SDK.

MCP 1.26.0 ClientSession.call_tool validates structured content against the
advertised output schema. This module also validates requests and parses a
completed tool result with `isError` into `ToolFailure`.
The SDK owns the stdio proxy and its bounded shutdown. The foreground server
inherits the caller environment, including LLVM_PROFILE_FILE; overrides win.
Harness output always lives outside the served workspace.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import threading
import time
import traceback
import xml.etree.ElementTree as ET
from collections.abc import (
    AsyncIterator,
    Callable,
    Coroutine,
    Iterator,
    Mapping,
    Sequence,
)
from contextlib import ExitStack, asynccontextmanager, contextmanager
from contextvars import ContextVar
from datetime import UTC, datetime
from pathlib import Path
from types import TracebackType
from typing import BinaryIO, NamedTuple, Self, TextIO, TypeAlias, cast

import psutil
import tomllib
from jsonschema import Draft202012Validator
from mcp import ClientSession, StdioServerParameters, types
from mcp.client.stdio import stdio_client

from rift_dev.check_mcp_conformance import build_server_binary
from rift_dev.commands import (
    REPOSITORY,
    Command,
    Drain,
    Process,
    owned_environment,
    termination_handler,
)
from rift_dev.log_records import Line, lines, newest_in_flight
from rift_dev.trace import Collector, nanoseconds

# `list` and `dict` are invariant, so a `list[JsonObject]` an assertion builds is
# not a `list[Json]` and cannot be passed where a JSON value is expected. The
# covariant `Sequence` and `Mapping` accept both, and every consumer narrows
# through `array_value` or `object_value` before it indexes or mutates.
Json: TypeAlias = (
    None | bool | int | float | str | Sequence["Json"] | Mapping[str, "Json"]
)
JsonObject: TypeAlias = dict[str, Json]
LOG_BYTES_MAX = 8 * 1024 * 1024
# The filter every Rift child under test runs with. It is set, never inherited, so
# an `RUST_LOG` in the developer's shell or the runner cannot change what a failed
# run left on stderr.
LOG_FILTER = "rift=info,rift_mcp=debug,rift_server=debug,rift_index=info"
# What a failure keeps of one stream: the newest bytes, inline. The whole stream is
# in its file next to the report.
EVIDENCE_TAIL_BYTES = 256 * 1024
# Newest persisted records `rift server logs` prints. A stop writes its `database.close`
# and `stop stage ended` records last, so they are among the newest; the corpus
# configuration keeps `page_records = 5000`, the same count.
RECORD_TAIL = 5000
# The newest bytes of one records file. Older bytes are replaced by a line naming
# how many were left out.
RECORDS_FILE_BYTES = 1024 * 1024
# The newest bytes of one record the window prints. A table of operations in flight
# carries a JSON array of its open entries.
WINDOW_RECORD_BYTES = 16 * 1024
EVIDENCE_SECONDS = 10.0
MESSAGE_BYTES_MAX = 16 * 1024 * 1024
PAGE_COUNT_MAX = 32
POLL_SECONDS = 0.05
STOP_SECONDS = 5.0
# The SDK session's own read timeout stands strictly inside the deadline the client puts
# around the same call. Equal bounds fire together, so a call that runs long comes back as
# a cancelled transport instead of the session's own refusal, and which of the two a test
# reports is a race. The client carries the session's bound plus room for that refusal.
SESSION_DEADLINE_MARGIN_SECONDS = 5.0
_GATE_DEADLINE: ContextVar[float | None] = ContextVar(
    "rift_gate_deadline", default=None
)


def object_value(value: object, context: str) -> JsonObject:
    """Require a JSON object at a decoded message boundary."""
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise AssertionError(f"{context}: expected an object, received {value!r}")
    return cast(JsonObject, value)


def array_value(value: Json, context: str) -> list[Json]:
    """Require an array before inspecting its members."""
    if not isinstance(value, list):
        raise TypeError(f"{context}: expected an array, received {value!r}")
    return value


def string_value(value: Json, context: str) -> str:
    """Require a nonempty string before using an emitted address."""
    if not isinstance(value, str) or not value:
        raise AssertionError(
            f"{context}: expected a nonempty string, received {value!r}"
        )
    return value


def require(condition: bool, detail: str) -> None:
    """Fail a gate even when Python runs with assertions disabled."""
    if not condition:
        raise AssertionError(detail)


def outside_workspace(path: Path, root: Path) -> None:
    """Reject output beneath the canonical served root, including symlinks."""
    require(
        not path.resolve().is_relative_to(root.resolve()),
        f"harness output must be outside the served workspace: {path}",
    )


def current_deadline() -> float | None:
    """The monotonic deadline of the gate the caller runs under, if any.

    A command a gate starts carries it through `Command.with_deadline`, so the
    command cannot outlive the gate's budget.
    """
    return _GATE_DEADLINE.get()


def candidate_binary(binary: Path | None, target: str | None = None) -> Path:
    """Use supplied bytes, or reuse the conformance runner's Cargo artifact discovery."""
    if binary is not None:
        require(
            target is None, "--target selects a build and cannot accompany --binary"
        )
        require(binary.is_file(), f"supplied binary does not exist: {binary}")
        return binary.resolve()
    return build_server_binary(release=True, target=target)


def workspace_version() -> str:
    """Read the expected version from the workspace's existing Cargo manifest."""
    with (REPOSITORY / "Cargo.toml").open("rb") as manifest:
        document = tomllib.load(manifest)
    return string_value(
        document["workspace"]["package"]["version"], "workspace.package.version"
    )


def verify_version(binary: Path, version: str) -> None:
    """Require the supplied executable to report exactly the expected release version."""
    expected = f"rift {version.removeprefix('v')}"
    observed = (
        Command(binary.resolve(), "--version")
        .with_deadline(current_deadline())
        .output()
        .strip()
    )
    require(observed == expected, f"expected {expected!r}, received {observed!r}")


def remaining_seconds(seconds: float) -> float:
    """Limit a synchronous wait to the current gate's remaining budget."""
    deadline = _GATE_DEADLINE.get()
    return (
        seconds
        if deadline is None
        else min(seconds, max(0.0, deadline - time.monotonic()))
    )


@asynccontextmanager
async def gate_deadline(name: str, seconds: float) -> AsyncIterator[None]:
    """Bound direct callers as well as CLI runs, including synchronous work.

    asyncio.timeout schedules cancellation on the event loop. The elapsed check
    also rejects work that blocked that loop until its timeout handle was removed.
    Nested gates keep the earlier deadline; cleanup retains its own process bounds.
    """
    if seconds <= 0:
        raise ValueError("gate deadline must be positive")
    started = time.monotonic()
    inherited = _GATE_DEADLINE.get()
    deadline = started + seconds
    if inherited is not None:
        deadline = min(deadline, inherited)
    budget = max(0.0, deadline - started)
    token = _GATE_DEADLINE.set(deadline)
    try:
        async with asyncio.timeout(budget):
            yield
        if time.monotonic() > deadline:
            raise TimeoutError(f"{name} exceeded its {budget:.3f}s deadline")
    finally:
        _GATE_DEADLINE.reset(token)


def run_gate(
    name: str, operation: Coroutine[None, None, None], junit: Path | None = None
) -> None:
    """Write a JUnit result on success or failure, preserving the original failure."""
    started = time.monotonic()
    failure: str | None = None
    try:
        asyncio.run(operation)
    except BaseException:
        failure = traceback.format_exc()
        raise
    finally:
        if junit is not None:
            write_junit(junit, name, time.monotonic() - started, failure)


def xml_text(value: str) -> str:
    """Keep bounded terminal output valid in XML text and attribute values."""
    return "".join(
        character
        for character in value[-LOG_BYTES_MAX:]
        if character.isprintable() or character in "\t\n\r"
    )


def write_junit(path: Path, name: str, seconds: float, failure: str | None) -> None:
    """Serialize one gate result with XML escaping and a bounded failure transcript."""
    suite = ET.Element(
        "testsuite",
        name=name,
        tests="1",
        failures=str(int(failure is not None)),
        time=f"{seconds:.6f}",
    )
    case = ET.SubElement(
        suite, "testcase", classname="rift.gate", name=name, time=f"{seconds:.6f}"
    )
    if failure is not None:
        finding = ET.SubElement(case, "failure", message=f"{name} failed")
        finding.text = xml_text(failure)
    path.parent.mkdir(parents=True, exist_ok=True)
    ET.ElementTree(suite).write(path, encoding="utf-8", xml_declaration=True)


def cut_notice(stream: str, limit: int) -> str:
    """The line a file ends with when its stream wrote past `limit` bytes."""
    return f"[rift-dev: {stream} cut at {limit} bytes; later output was dropped]\n"


def tail_text(
    text: str, path: Path | None = None, limit: int = EVIDENCE_TAIL_BYTES
) -> str:
    """The newest `limit` bytes of `text`, naming `path` when older bytes are left out.

    A stream held only in memory has no file, so it states the omitted count alone.
    """
    encoded = text.encode("utf-8")
    if len(encoded) <= limit:
        return text
    kept = encoded[-limit:].decode("utf-8", errors="replace")
    omitted = len(encoded) - limit
    where = f"are in {path}" if path is not None else "were left out"
    return f"[{omitted} earlier bytes {where}]\n{kept}"


def utc_now() -> str:
    """The current UTC time, to the millisecond, as ISO 8601."""
    return datetime.now(UTC).isoformat(timespec="milliseconds")


# Runs `rift server logs` with these arguments and returns its stdout.
LogsReader: TypeAlias = Callable[[Sequence[str]], str]


def logs_arguments(
    until: str, *, since: str | None = None, tail: int = RECORD_TAIL
) -> list[str]:
    """The arguments of a `rift server logs` read of records recorded before `until`.

    `since` and `until` are RFC 3339 instants, as `utc_now` prints them. `--since`
    keeps records at or after the instant and `--until` keeps those before it.
    """
    arguments = ["server", "logs", "--tail", str(tail)]
    if since is not None:
        arguments += ["--since", since]
    return [*arguments, "--until", until]


def cut_record(text: str) -> str:
    """One printed record cut to `WINDOW_RECORD_BYTES`, with the cut stated."""
    encoded = text.encode("utf-8")
    if len(encoded) <= WINDOW_RECORD_BYTES:
        return text
    kept = encoded[:WINDOW_RECORD_BYTES].decode("utf-8", errors="ignore")
    return f"{kept} [{len(encoded) - WINDOW_RECORD_BYTES} later bytes were left out]"


# Metric points and spans one failure window prints, the newest of the window.
WINDOW_POINTS_MAX = 200
WINDOW_SPANS_MAX = 100
# The field a printed log record names its MCP request with (`REQUEST_LABEL` in
# `crates/rift-tracing/src/render.rs`).
RECORD_REQUEST_LABEL = "req"
NO_POINTS = "no metric points received"
NO_SPANS = "no spans received"


def telemetry_notes(
    collector: Collector,
    *,
    since: str | None,
    until: str,
    records: Sequence[Line] = (),
) -> list[str]:
    """What `collector` received inside a failure's window, as notes.

    The window `[since, until)` is the only key metric points and log records share:
    a point carries `time_unix_nano`, a record its recorded time. A point carries no
    request. A span carrying `request_id` names the count of `records` whose `req` is
    the same, so a span and the records of its request read together. Each part prints
    `WINDOW_POINTS_MAX` or `WINDOW_SPANS_MAX` lines at most, the newest, oldest
    first; a collector that received nothing says so in one line, and what its bounds
    dropped is stated whenever anything was.
    """
    lower = None if since is None else nanoseconds(since)
    upper = nanoseconds(until)
    start = since or "the first point received"
    notes: list[str] = []
    if collector.metrics.received == 0:
        notes.append(NO_POINTS)
    else:
        points = collector.metrics.between(lower, upper)
        shown = points[-WINDOW_POINTS_MAX:]
        heading = (
            f"metric points from {start} until {until}, the newest "
            f"{WINDOW_POINTS_MAX} at most"
        )
        body = (
            "\n".join(cut_record(point.line()) for point in shown)
            if shown
            else (
                f"(no metric points in the window; {collector.metrics.received} "
                "received in all)"
            )
        )
        cut = (
            f"[{len(points) - len(shown)} older points of the window were left out]\n"
            if len(points) > len(shown)
            else ""
        )
        notes.append(f"{heading}:\n{cut}{body}")
    if collector.spans.received == 0:
        notes.append(NO_SPANS)
    else:
        requests: dict[str, int] = {}
        for record in records:
            request = record.label(RECORD_REQUEST_LABEL)
            if request:
                requests[request] = requests.get(request, 0) + 1
        spans = collector.spans.between(lower, upper)
        shown_spans = spans[-WINDOW_SPANS_MAX:]
        heading = (
            f"spans ending from {start} until {until}, the newest {WINDOW_SPANS_MAX} at "
            "most; records= counts the window's log records of the span's request_id"
        )
        body = (
            "\n".join(
                cut_record(
                    span.line(
                        None
                        if span.request_id is None
                        else requests.get(span.request_id, 0)
                    )
                )
                for span in shown_spans
            )
            if shown_spans
            else f"(no spans in the window; {collector.spans.received} received in all)"
        )
        cut = (
            f"[{len(spans) - len(shown_spans)} older spans of the window were left out]\n"
            if len(spans) > len(shown_spans)
            else ""
        )
        notes.append(f"{heading}:\n{cut}{body}")
    dropped = collector.dropped()
    if dropped.any():
        counts = " ".join(
            f"{reason}={count}" for reason, count in dropped.counts().items() if count
        )
        notes.append(f"the collector's bounds dropped: {counts}")
    return notes


def collector_counts(collector: Collector | None) -> JsonObject | None:
    """What `collector` received and dropped; None when none started."""
    if collector is None:
        return None
    return {
        "points": collector.metrics.received,
        "spans": collector.spans.received,
        "dropped": dict(collector.dropped().counts()),
    }


# Where each runner keeps its server log, served workspace, and collector counts, in a
# directory of its own; the CI job uploads this directory.
INTEGRATION_DIRECTORY = REPOSITORY / "target" / "integration"


def retained_directory(runner: str) -> Path:
    """`target/integration/<runner>/`, created empty, kept after the runner ends."""
    directory = INTEGRATION_DIRECTORY / runner
    shutil.rmtree(directory, ignore_errors=True)
    directory.mkdir(parents=True)
    return directory


def retain_collector(directory: Path, collector: Collector | None) -> None:
    """Write `collector_counts` to `collector.json` in `directory`."""
    (directory / "collector.json").write_text(
        json.dumps(collector_counts(collector), sort_keys=True) + "\n",
        encoding="utf-8",
    )


def collector_line(collector: Collector | None) -> str:
    """The `collector` entry of a runner's report as one line."""
    return "collector: " + json.dumps(collector_counts(collector), sort_keys=True)


def failure_window(
    read: LogsReader,
    *,
    since: str | None,
    lower_bound: str,
    until: str,
    file: Path | None = None,
    collector: Collector | None = None,
) -> list[str]:
    """The records of a failure's window, as notes for the failure to carry.

    Two reads, each `RECORD_TAIL` records at most, each failing into a note and
    never raising, so a pass cannot turn into a failure here and a failure keeps
    its own error:

    - the log records from `since` to `until`, in full, through `tail_text`; the
      note states `lower_bound`, what `since` is, and when the read holds the newest
      `RECORD_TAIL` records the older ones of the window are left out;
    - the newest `operations in flight` record before `until`, wherever it was
      recorded, when it is among the newest `RECORD_TAIL` records before `until`.

    With a `collector`, the `telemetry_notes` of the same window follow.
    """
    heading = (
        f"failure window: log records from {since or 'the oldest kept record'} "
        f"({lower_bound}) "
        f"until {until}, the newest {RECORD_TAIL} at most"
    )
    notes: list[str] = []
    window: list[Line] = []
    try:
        text = read(logs_arguments(until, since=since))
    except (OSError, RuntimeError, ValueError) as error:
        notes.append(f"{heading}\nunavailable: {error}")
    else:
        window = list(lines(text))
        cut = (
            f"[the window holds {RECORD_TAIL} records; older records of the window "
            "were left out]\n"
            if len(window) >= RECORD_TAIL
            else ""
        )
        body = tail_text(text, file) if text else "(no records in the window)\n"
        notes.append(f"{heading}\n{cut}{body}")
    try:
        before = list(lines(read(logs_arguments(until))))
    except (OSError, RuntimeError, ValueError) as error:
        notes.append(f"newest operations in flight unavailable: {error}")
    else:
        flight = newest_in_flight(before)
        notes.append(
            "newest operations in flight record before "
            f"{until}:\n"
            + (
                cut_record(flight.text)
                if flight is not None
                else f"(none among the newest {RECORD_TAIL} records)"
            )
        )
    if collector is not None:
        notes.extend(
            telemetry_notes(collector, since=since, until=until, records=window)
        )
    return notes


@contextmanager
def stderr_log(
    path: Path | None = None, *, output: BinaryIO | None = None
) -> Iterator[TextIO]:
    """Give the SDK a real stderr handle backed by the shared bounded drain.

    The SDK must close its process before this context exits. On overflow the
    drain stops reading, so the child's next write meets the request deadline.
    """
    read_fd, write_fd = os.pipe()
    with (
        os.fdopen(read_fd, "rb", buffering=0) as source,
        os.fdopen(write_fd, "w", encoding="utf-8") as destination,
    ):
        drain = Drain(source, LOG_BYTES_MAX, threading.Event(), output=output)
        reader = threading.Thread(target=drain.read, daemon=True)
        reader.start()
        try:
            yield destination
        finally:
            destination.close()
            reader.join(timeout=STOP_SECONDS)
            require(not reader.is_alive(), "SDK stderr did not close within its bound")
            if path is not None:
                notice = (
                    cut_notice("SDK stderr", LOG_BYTES_MAX)
                    if len(drain.data) > LOG_BYTES_MAX
                    else ""
                )
                path.write_bytes(drain.data[:LOG_BYTES_MAX] + notice.encode("utf-8"))
        if drain.error is not None:
            raise RuntimeError("SDK stderr collection failed") from drain.error


FAILURE_TITLE = re.compile(r"([0-9]+) (errors?)")
INDENT_UNIT = "\t"
FAILURE_HEAD = re.compile(re.escape(INDENT_UNIT) + r"(\S+) · retry (\S+)")
FAILURE_LIMIT = re.compile(r"limit (\S+): (\d+) over (\d+)")
FAILURE_LINE_INDENT = INDENT_UNIT * 2
FAILURE_ESCAPE = re.compile(r"\\(?:([nrt])|u\{([0-9A-Fa-f]+)\})")
FAILURE_ESCAPES = {"n": "\n", "r": "\r", "t": "\t"}


class FailureLimit(NamedTuple):
    """Limit evidence of a failure: `limit <field>: <required> over <limit>`."""

    field: str
    required: int
    limit: int


class FailureCause(NamedTuple):
    """One cause entry: its head code and retry directive, and its message."""

    code: str
    message: str
    retry: str


class ToolFailure(Exception):
    """A Rift operating failure returned as a completed tool result with `isError`.

    `text` is the raw content. `diagnostics` keeps the lines of the first entry
    after its message and limit line as raw text, without the two-level indent.
    """

    def __init__(
        self,
        tool: str,
        text: str,
        code: str,
        message: str,
        retry: str,
        limit: FailureLimit | None = None,
        causes: Sequence[FailureCause] = (),
        diagnostics: Sequence[str] = (),
    ) -> None:
        """Keep the parsed lines and the raw text."""
        super().__init__(f"{tool} failed: {text}")
        self.tool = tool
        self.text = text
        self.code = code
        self.message = message
        self.retry = retry
        self.limit = limit
        self.causes = list(causes)
        self.diagnostics = list(diagnostics)


def unescape_message(line: str) -> str:
    """Undo `\\n`, `\\r`, `\\t`, and `\\u{HEX}`; other backslashes stay."""

    def replace(found: re.Match[str]) -> str:
        short, code_point = found.groups()
        return FAILURE_ESCAPES[short] if short else chr(int(code_point, 16))

    return FAILURE_ESCAPE.sub(replace, line)


def parse_failure(tool: str, text: str) -> ToolFailure:
    """Parse the failure text of a tool result; the only place that decodes it.

    Line 1 is the title `N error(s)`. Each entry is a head line at one level,
    `<code> · retry <directive>`, then lines at two levels. A level is one tab. The first entry is the
    failure: its message, an optional `limit` line, then raw diagnostic lines.
    Every later entry is a cause and has its message only. N must equal the
    number of entries.
    """
    lines = text.rstrip("\n").split("\n")
    title = FAILURE_TITLE.fullmatch(lines[0])
    require(title is not None, f"{tool} failed without an errors title: {text!r}")
    assert title is not None
    count = int(title[1])
    require(
        (title[2] == "error") == (count == 1),
        f"{tool} failure title has the wrong noun number: {lines[0]!r}",
    )
    entries: list[tuple[str, str, list[str]]] = []
    for line in lines[1:]:
        if line.startswith(FAILURE_LINE_INDENT):
            require(
                len(entries) > 0, f"{tool} failure has a line before an entry: {line!r}"
            )
            entries[-1][2].append(line[len(FAILURE_LINE_INDENT) :])
            continue
        head = FAILURE_HEAD.fullmatch(line)
        require(head is not None, f"{tool} failure has a bad entry head: {line!r}")
        assert head is not None
        entries.append((head[1], head[2], []))
    require(
        len(entries) == count,
        f"{tool} failure title counts {count} entries, text has {len(entries)}: {text!r}",
    )
    for code, _retry, body in entries:
        require(
            len(body) > 0 and body[0] != "",
            f"{tool} failure entry {code} lacks a message: {text!r}",
        )
    code, retry, body = entries[0]
    limit: FailureLimit | None = None
    diagnostics: list[str] = []
    for line in body[1:]:
        if line.startswith("limit "):
            found = FAILURE_LIMIT.fullmatch(line)
            require(
                found is not None and limit is None,
                f"{tool} failure has a bad limit line: {line!r}",
            )
            assert found is not None
            limit = FailureLimit(found[1], int(found[2]), int(found[3]))
        else:
            diagnostics.append(line)
    causes: list[FailureCause] = []
    for cause_code, cause_retry, cause_body in entries[1:]:
        require(
            len(cause_body) == 1,
            f"{tool} failure cause {cause_code} has more than a message: {text!r}",
        )
        causes.append(
            FailureCause(cause_code, unescape_message(cause_body[0]), cause_retry)
        )
    return ToolFailure(
        tool, text, code, unescape_message(body[0]), retry, limit, causes, diagnostics
    )


class Client:
    """Validate calls against the schemas advertised by one SDK session."""

    def __init__(self, session: ClientSession, session_seconds: float = 120.0) -> None:
        """Bound each call by `session_seconds` plus the session refusal's own room."""
        self.session = session
        self.call_seconds = session_seconds + SESSION_DEADLINE_MARGIN_SECONDS
        self.tools: dict[str, types.Tool] = {}
        self.exercised: set[str] = set()

    async def initialize(self) -> None:
        """Reject missing schemas or a tool listing that exceeds its page bound."""
        async with asyncio.timeout(self.call_seconds):
            await self.session.initialize()
            cursor: str | None = None
            for _ in range(PAGE_COUNT_MAX):
                page = await self.session.list_tools(
                    params=types.PaginatedRequestParams(cursor=cursor)
                )
                for tool in page.tools:
                    require(tool.name not in self.tools, f"duplicate tool: {tool.name}")
                    if tool.output_schema is None:
                        raise AssertionError(f"{tool.name} has no output schema")
                    for schema in (tool.input_schema, tool.output_schema):
                        require(
                            schema is not None and schema.get("type") == "object",
                            f"{tool.name}: MCP requires object schemas",
                        )
                        Draft202012Validator.check_schema(schema)
                    self.tools[tool.name] = tool
                cursor = page.next_cursor
                if cursor is None:
                    require(bool(self.tools), "tools/list returned no tools")
                    return
        raise AssertionError(f"tools/list exceeded {PAGE_COUNT_MAX} pages")

    async def call(self, name: str, arguments: JsonObject) -> JsonObject:
        """Validate requests and structured answers; raise `ToolFailure` on `isError`.

        A failed result carries no `structuredContent`, so it never meets the
        success `outputSchema`. JSON-RPC errors still raise as the SDK raises them.
        """
        tool = self.tools[name]
        Draft202012Validator(tool.input_schema).validate(arguments)
        async with asyncio.timeout(self.call_seconds):
            result = await self.session.call_tool(name, arguments)
        if result.is_error:
            raise parse_failure(
                name,
                "\n".join(
                    block.text
                    for block in result.content
                    if isinstance(block, types.TextContent)
                ),
            )
        answer = object_value(result.structured_content, name)
        require(
            len(json.dumps(answer).encode()) <= MESSAGE_BYTES_MAX,
            f"{name} result exceeds {MESSAGE_BYTES_MAX} bytes",
        )
        if tool.output_schema is None:
            raise AssertionError(f"{name} has no output schema")
        Draft202012Validator(tool.output_schema).validate(answer)
        self.exercised.add(name)
        return answer

    async def resource(self, uri: str) -> JsonObject:
        """Select the JSON document from the SDK resource envelope.

        The envelope holds one or two contents for `uri`: exactly one
        `application/json`, and when there are two, the other is `text/plain`.
        """
        async with asyncio.timeout(self.call_seconds):
            result = await self.session.read_resource(uri)
        require(
            len(result.contents) in (1, 2),
            f"{uri}: expected one or two resource contents",
        )
        texts: list[types.TextResourceContents] = []
        for content in result.contents:
            if not isinstance(content, types.TextResourceContents):
                raise TypeError(f"{uri}: expected text content")
            require(str(content.uri) == uri, f"{uri}: resource returned {content.uri}")
            texts.append(content)
        documents = [text for text in texts if text.mime_type == "application/json"]
        require(len(documents) == 1, f"{uri}: expected one application/json content")
        require(
            len(texts) == 1
            or {text.mime_type for text in texts} == {"text/plain", "application/json"},
            f"{uri}: expected text/plain beside application/json",
        )
        document = documents[0]
        require(
            len(document.text.encode()) <= MESSAGE_BYTES_MAX,
            f"{uri}: document too large",
        )
        return object_value(json.loads(document.text), uri)

    def require_complete(self, read_tools: set[str]) -> None:
        """Require every selected read tool to be advertised and exercised."""
        require(
            read_tools <= set(self.tools),
            f"missing tools: {sorted(read_tools - set(self.tools))}",
        )
        require(
            read_tools <= self.exercised,
            f"unexercised tools: {sorted(read_tools - self.exercised)}",
        )


def process_alive(process: psutil.Process) -> bool:
    """Check a captured process identity while tolerating exit between observations."""
    try:
        return process.is_running() and process.status() != psutil.STATUS_ZOMBIE
    except psutil.NoSuchProcess:
        return False


class Server:
    """Own a foreground server, its external log, and observed descendants."""

    # UTC time the server was started at, as `utc_now` prints it; empty before `start`.
    started_at: str = ""

    def __init__(
        self,
        binary: Path,
        root: Path,
        log_path: Path,
        *,
        startup_seconds: float = 120.0,
        env: Mapping[str, str] | None = None,
        output: BinaryIO | None = None,
        collector: Collector | None = None,
    ) -> None:
        """`collector`, when given, receives the server's OTLP export: its
        `environment()` is set below `env`, and the failure window prints what it
        received."""
        outside_workspace(log_path, root)
        require(startup_seconds > 0, "startup timeout must be positive")
        self.binary = binary.resolve()
        self.root = root
        self.log_path = log_path
        self.startup_seconds = startup_seconds
        self.env = dict(os.environ)
        self.env["RUST_LOG"] = LOG_FILTER
        self.collector = collector
        if collector is not None:
            self.env.update(collector.environment())
        self.env.update(env or {})
        self.output = output
        self.process: Process
        self.port = 0
        self._process_stack = ExitStack()
        self._owner: psutil.Process | None = None
        self._reader: threading.Thread | None = None
        self._log_failure: BaseException | None = None
        self._descendants: list[psutil.Process] = []
        self._stopped = False
        self._proxy_logs: list[Path] = []
        self._output_cut = False

    def start(self, *, wait_for_publication: bool = True) -> Self:
        """Start once; callers may inspect output before awaiting publication."""
        require(self._reader is None, "server has already been started")
        self.started_at = utc_now()
        require(
            not (self.root / ".rift" / "server.json").exists(),
            "workspace already has a server document; use a disposable workspace",
        )
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        try:
            self._process_stack.enter_context(termination_handler())
            server = (
                Command(
                    self.binary, "server", "start", "--foreground", "--auth", "skip"
                )
                .with_cwd(self.root)
                .with_environment(self.env)
            )
            self.process = self._process_stack.enter_context(server.spawn())
            try:
                self._owner = psutil.Process(self.process.pid)
            except psutil.NoSuchProcess:
                self._owner = None
            self._reader = threading.Thread(target=self._drain, daemon=True)
            self._reader.start()
            if wait_for_publication:
                self.await_publication()
            return self
        except BaseException:
            self.close()
            raise

    def _drain(self) -> None:
        """Count at most LOG_BYTES_MAX + 1 bytes; a flooding child then blocks."""
        try:
            if self.process.stdout is None:
                raise AssertionError("server stdout pipe is missing")
            with self.log_path.open("wb") as log:
                remaining = LOG_BYTES_MAX
                while remaining >= 0:
                    chunk = os.read(
                        self.process.stdout.fileno(), min(65536, remaining + 1)
                    )
                    if not chunk:
                        return
                    if len(chunk) > remaining:
                        self._output_cut = True
                        log.write(cut_notice("server output", LOG_BYTES_MAX).encode())
                    require(
                        len(chunk) <= remaining,
                        f"server output exceeded {LOG_BYTES_MAX} bytes",
                    )
                    log.write(chunk)
                    log.flush()
                    if self.output is not None:
                        self.output.write(chunk)
                        self.output.flush()
                    remaining -= len(chunk)
        except (OSError, ValueError, AssertionError) as failure:
            self._log_failure = failure

    def read_log(self) -> str:
        """Read only the bounded output this server owns."""
        if not self.log_path.exists():
            return ""
        with self.log_path.open("rb") as log:
            text = log.read(LOG_BYTES_MAX).decode("utf-8", errors="replace")
        if self._output_cut:
            text += cut_notice("server output", LOG_BYTES_MAX)
        return text

    @property
    def records_path(self) -> Path:
        """Where `records` writes the persisted log records, beside the server log."""
        return self.log_path.with_suffix(".records.log")

    @property
    def proxy_logs(self) -> tuple[Path, ...]:
        """The stderr file of every `rift mcp` connection this server served."""
        return tuple(self._proxy_logs)

    @property
    def output_cut(self) -> bool:
        """Whether the server wrote past `LOG_BYTES_MAX` and its log stops there."""
        return self._output_cut

    def read_records(self) -> str:
        """Read the persisted log records through `rift server logs` into `records_path`.

        The records live in the workspace's `.rift/metrics`, which the CLI reads
        without a server, so a stopped or killed server still answers. The file
        keeps the newest `RECORDS_FILE_BYTES`, with a line naming the bytes left
        out. Raises `OSError`, `RuntimeError`, or `ValueError` when the read fails.
        """
        text = (
            Command(self.binary, "server", "logs", "--tail", str(RECORD_TAIL))
            .with_cwd(self.root)
            .with_environment(self.env)
            .with_timeout(EVIDENCE_SECONDS)
            .with_output_limit(LOG_BYTES_MAX)
            .output()
        )
        self.records_path.write_bytes(
            tail_text(text, None, RECORDS_FILE_BYTES).encode("utf-8")
        )
        return text

    def records(self) -> str:
        """`read_records` for evidence: bounded and never raising.

        A failed read is reported as text so collecting evidence cannot replace
        the failure it explains.
        """
        path = self.records_path
        try:
            text = self.read_records()
        except (OSError, RuntimeError, ValueError) as error:
            return f"persisted log records unavailable: {error}"
        return f"persisted log records ({path}):\n{tail_text(text, path)}"

    @property
    def window_path(self) -> Path:
        """Where `window` keeps the failure window, beside the server log."""
        return self.log_path.with_suffix(".window.log")

    def read_logs(self, arguments: Sequence[str]) -> str:
        """`rift` with `arguments` in this workspace, its stdout bounded to `LOG_BYTES_MAX`."""
        return (
            Command(self.binary, *arguments)
            .with_cwd(self.root)
            .with_environment(self.env)
            .with_timeout(EVIDENCE_SECONDS)
            .with_output_limit(LOG_BYTES_MAX)
            .output()
        )

    def window(
        self, since: str | None = None, lower_bound: str | None = None
    ) -> list[str]:
        """The failure window of this server as notes, from `since` until now.

        `since` defaults to the server's start, a lower bound the server's own
        clock confirms. A caller that knows a later lower bound, such as the end
        of the last action the runner recorded, passes it with a `lower_bound` that
        says what it is. The whole first read is kept in `window_path` beside the
        server log, bounded by `RECORDS_FILE_BYTES`; the note carries the newest
        `EVIDENCE_TAIL_BYTES` of it.
        """
        first = since or self.started_at or None
        until = utc_now()
        kept: list[str] = []

        def read(arguments: Sequence[str]) -> str:
            text = self.read_logs(arguments)
            if not kept:
                kept.append(text)
                self.window_path.write_bytes(
                    tail_text(text, None, RECORDS_FILE_BYTES).encode("utf-8")
                )
            return text

        return failure_window(
            read,
            since=first,
            lower_bound=lower_bound or "the server's start",
            until=until,
            file=self.window_path,
            collector=self.collector,
        )

    def evidence(
        self, since: str | None = None, lower_bound: str | None = None
    ) -> list[str]:
        """What a failure keeps: server stderr, each proxy's stderr, the persisted
        records, and the failure window.

        Each part is one note; a stream longer than `EVIDENCE_TAIL_BYTES` keeps its
        newest bytes and names the file holding the rest. `since` and `lower_bound`
        are those of `window`.
        """
        notes = [
            f"server stderr ({self.log_path}):\n"
            + tail_text(self.read_log(), self.log_path)
        ]
        for proxy in self._proxy_logs:
            text = (
                proxy.read_bytes().decode("utf-8", errors="replace")
                if proxy.exists()
                else ""
            )
            notes.append(f"rift mcp stderr ({proxy}):\n" + tail_text(text, proxy))
        notes.append(self.records())
        notes.extend(self.window(since, lower_bound))
        return notes

    def await_publication(self) -> int:
        """Wait at most startup_seconds, checking process ownership in server.json."""
        timeout_seconds = remaining_seconds(self.startup_seconds)
        deadline = time.monotonic() + timeout_seconds
        document = self.root / ".rift" / "server.json"
        while time.monotonic() < deadline:
            self.check_running()
            if document.exists():
                with document.open("rb") as source:
                    encoded = source.read(65537)
                require(len(encoded) <= 65536, "server document exceeded 65536 bytes")
                value = object_value(json.loads(encoded), "server.json")
                require(
                    value.get("pid") == self.process.pid,
                    "server.json names another process",
                )
                port = value.get("port")
                require(
                    type(port) is int and 0 < port < 65536,
                    "server.json has an invalid port",
                )
                self.port = cast(int, port)
                return self.port
            time.sleep(POLL_SECONDS)
        raise AssertionError(
            f"server did not publish within {timeout_seconds}s\n{self.read_log()}"
        )

    def _check_log(self) -> None:
        if self._log_failure is not None:
            raise RuntimeError("server log collection failed") from self._log_failure

    def check_running(self) -> None:
        """Fail immediately on an exited server or failed log collection."""
        self._check_log()
        require(
            self.process.poll() is None,
            f"server exited {self.process.returncode}\n{self.read_log()}",
        )

    @asynccontextmanager
    async def connect(
        self, call_seconds: float = 120.0, *, log_path: Path | None = None
    ) -> AsyncIterator[Client]:
        """Connect through the SDK's real `rift mcp` stdio subprocess.

        `call_seconds` is the session's own read timeout; the client's deadline stands
        above it by SESSION_DEADLINE_MARGIN_SECONDS. An explicit log_path keeps
        concurrent connections' stderr files separate.
        """
        proxy_log = log_path or self.log_path.with_suffix(".mcp.log")
        outside_workspace(proxy_log, self.root)
        self._proxy_logs.append(proxy_log)
        with (
            stderr_log(proxy_log, output=self.output) as log,
            owned_environment(self.env) as environment,
        ):
            parameters = StdioServerParameters(
                command=str(self.binary),
                args=["mcp"],
                cwd=str(self.root),
                env=environment,
            )
            async with (
                stdio_client(parameters, errlog=log) as (read, write),
                ClientSession(read, write, call_seconds) as session,
            ):
                client = Client(session, call_seconds)
                await client.initialize()
                yield client
                self._check_log()
                if not self._stopped:
                    self.check_running()

    def _observe_descendants(self) -> None:
        try:
            if self._owner is not None:
                self._descendants.extend(self._owner.children(recursive=True))
        except psutil.NoSuchProcess:
            pass

    def stop(self, timeout_seconds: float = STOP_SECONDS) -> None:
        """Require CLI stop and owned process exit within one deadline."""
        started = time.monotonic()
        deadline = started + timeout_seconds
        self._observe_descendants()
        budget = remaining_seconds(max(0.0, deadline - time.monotonic()))
        require(budget > 0, "server stop exceeded its deadline")
        Command(self.binary, "server", "stop").with_cwd(self.root).with_environment(
            self.env
        ).with_timeout(budget).with_deadline(current_deadline()).output()
        # The CLI waits for election release. Process exit may follow it.
        try:
            self.process.wait(
                timeout=remaining_seconds(max(0.0, deadline - time.monotonic()))
            )
        except TimeoutError as error:
            raise AssertionError(
                "server stop returned while its process remained alive"
            ) from error
        alive = [process.pid for process in self._descendants if process_alive(process)]
        require(not alive, f"server stop left child processes alive: {alive}")
        require(
            self.process.returncode == 0,
            f"server exited {self.process.returncode}\n{self.read_log()}",
        )

        self._check_log()
        require(
            time.monotonic() <= deadline,
            "server stop exceeded its deadline",
        )
        self._stopped = True

    def close(self) -> None:
        """Close the shared process owner, then join its bounded log reader."""
        self._process_stack.close()
        if self._reader is None:
            return
        self._reader.join(timeout=STOP_SECONDS)
        require(not self._reader.is_alive(), "server log reader did not stop")
        if self.process.stdout is not None:
            self.process.stdout.close()

    def __enter__(self) -> Self:
        return self.start()

    def __exit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()
