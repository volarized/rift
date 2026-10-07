"""Run nextest beside an in-memory OTLP collector and report each failed test from it.

Every process a test spawns exports to the collector: the runner hands nextest the
collector's environment (`Collector.environment()`), the test process inherits it, and
the Rust harness adds `test.case.name` through `OTEL_RESOURCE_ATTRIBUTES` to every
`rift` process it starts (`crates/rift/tests/harness.rs`). `CaseStore` files what
arrives under that name. The runner reads nextest's status lines as they print: a test
that passes drops what its processes sent, a test that fails keeps it.

After nextest returns, one report per failed test prints and is written under
`target/integration/nextest/`, which CI uploads. A report reads top to bottom without
other files:

- the test, the OS and runner, the command, nextest's status and duration, and the
  profile's slow timeout;
- one timeline, by time, of the log records, span begins and ends, and metric points
  the test's processes sent, each line naming its process and request;
- the last value of every instrument series;
- the operations opened with no end received, and the newest `operations in flight`
  record;
- the test's own stdout and stderr as nextest printed them under its failure, which
  hold the records of every `ScopedRecorder` the test installed: the runner sets
  `RIFT_SCOPED_RECORDER_STREAM`, so each record prints as it is recorded and a test
  nextest ends at its timeout still leaves them;
- each process the harness registered: its exit status and the tail of its stderr,
  from the failure window directory `target/nextest/<profile>/failure-windows/`;
- what the collector received, dropped, and could not attribute.

Each part prints at most its named bound and says what the bound cut.
"""

from __future__ import annotations

import asyncio
import json
import math
import os
import platform
import re
import sys
import time
from collections.abc import Iterable, Sequence
from dataclasses import asdict, dataclass, field
from pathlib import Path
from statistics import median

import tomllib

from rift_dev.commands import REPOSITORY, Command, CommandFailed
from rift_dev.progress import finish, start
from rift_dev.trace import (
    EXPORT_REQUESTS_MAX,
    PID_KEY,
    SPAN_REQUEST_KEY,
    Attributes,
    CaseStore,
    CaseTelemetry,
    Collector,
    LogEntry,
    MetricPoint,
    RequestSnapshot,
    Scope,
    SpanRecord,
    collector,
    fields_text,
    stamp,
)

# One nextest status line: `        FAIL [  15.614s] (2292/4226) rift::server_cli name`.
# A retried attempt opens with `TRY <n>`; a stress run adds `[ 47/200]` before the
# counters in parentheses.
STATUS_LINE = re.compile(
    r"^\s*(?:TRY \d+ )?(?P<status>PASS|FAIL|TIMEOUT|SIGSEGV|SIGABRT|SIGBUS|SIGKILL|"
    r"SIGTERM|ABORT|LEAK-FAIL|LEAK|FLAKY \d+/\d+)\s+\[\s*(?P<seconds>[0-9.]+)s\]\s+"
    r"(?:\[\s*(?P<iteration>\d+)/\d+\]\s+)?(?:\([^)]*\)\s+)*"
    r"(?P<binary>\S+)\s+(?P<test>\S+)\s*$"
)
# The OpenTelemetry specification's "Disable the SDK for all signals": "true" makes the
# test processes themselves export nothing (`crates/rift-tracing/src/otlp.rs`).
SDK_DISABLED = "OTEL_SDK_DISABLED"
# Makes each `ScopedRecorder` print every record to stderr as it is recorded
# (`crates/rift-tracing/src/recorder.rs`, `SCOPED_RECORDER_STREAM_VARIABLE`).
RECORDER_STREAM = "RIFT_SCOPED_RECORDER_STREAM"
# The header nextest opens a test's captured stdout or stderr with, under its status line.
OUTPUT_HEADER = re.compile(r"^  (?:stdout|stderr) ───")
# The line nextest opens its final summary with.
SUMMARY_LINE = re.compile(r"^\s*Summary \[")
# Statuses that end a test without a failure.
PASSED = ("PASS", "LEAK", "FLAKY")
# The report directory below the repository, which CI uploads.
REPORT_DIRECTORY = REPOSITORY / "target" / "integration" / "nextest"
REPORT_DIRECTORY_ENV = "RIFT_TEST_REPORT_DIRECTORY"
# The failure window directory of one profile, below the repository (`harness.rs`).
WINDOW_DIRECTORY = "target/nextest/{profile}/failure-windows"
# Lines one report prints per part, the newest of each.
TIMELINE_LINES_MAX = 3_000
INSTRUMENT_LINES_MAX = 400
OPEN_LINES_MAX = 100
OUTPUT_LINES_MAX = 2_000
STDERR_BYTES_MAX = 64 * 1024
# Bytes retained from one nextest invocation's raw output.
RUN_LOG_BYTES_MAX = 16 * 1024 * 1024
# How long the runner waits for nextest to exit once its output closes.
EXIT_WAIT_SECONDS = 60.0
# Decoded telemetry lines retained for each passing test and their run artifact.
CASE_EVIDENCE_LINES_MAX = 40
CASE_EVIDENCE_BYTES_MAX = 16 * 1024 * 1024
# Characters one report line keeps.
LINE_CHARS_MAX = 2_000
# The log record message an operation declared with `open = true` opens with
# (`crates/rift-tracing/src/span.rs`), and the table of operations in flight's.
OPENED_MESSAGE = "operation opened"
IN_FLIGHT_MESSAGE = "operations in flight"
MEASUREMENT_FIELDS: dict[str, tuple[tuple[str, ...], tuple[str, ...]]] = {
    "operation_record_cost": (("threads",), ("operation_ns", "event_ns")),
    "record_path_cost": (
        ("meter",),
        ("counter_ns_per_record", "histogram_ns_per_record"),
    ),
    "output allocation measurement": (
        ("answer", "example", "representation"),
        (
            "allocations",
            "deallocations",
            "reallocations",
            "bytes_allocated",
            "bytes_deallocated",
            "bytes_reallocated",
            "content_bytes",
            "structured_content_bytes",
            "combined_response_bytes",
        ),
    ),
}


def measurement_summary(logs: Iterable[LogEntry]) -> list[str]:
    """Summarize recorded T-149 samples without combining separate processes."""
    groups: dict[
        tuple[str, tuple[str, ...], str],
        dict[str, list[tuple[float, int]]],
    ] = {}
    incomplete = 0
    for entry in logs:
        if entry.body not in MEASUREMENT_FIELDS:
            continue
        group_keys, fields = MEASUREMENT_FIELDS[entry.body]
        attributes = dict(entry.attributes)
        group_values = tuple(attributes.get(name) for name in group_keys)
        instance = entry.instance
        if any(value is None for value in group_values) or instance is None:
            incomplete += 1
            continue
        key = (
            entry.body,
            tuple(value for value in group_values if value is not None),
            instance,
        )
        samples = groups.setdefault(key, {name: [] for name in fields})
        row_incomplete = False
        for name in fields:
            try:
                value = float(attributes[name])
            except (KeyError, ValueError):
                row_incomplete = True
                continue
            if not math.isfinite(value):
                row_incomplete = True
                continue
            samples[name].append((value, entry.time_unix_nano))
        incomplete += int(row_incomplete)

    if not groups and incomplete == 0:
        return []
    lines = ["---- T-149 measurement medians ----"]
    for (measurement, group_values, instance), samples in sorted(groups.items()):
        group_keys = MEASUREMENT_FIELDS[measurement][0]
        labels = " ".join(
            f"{name}={value}"
            for name, value in zip(group_keys, group_values, strict=True)
        )
        lines.append(f"{measurement} {labels} service.instance.id={instance}")
        for name, values in samples.items():
            if not values:
                lines.append(f"{name}: no complete samples")
                continue
            times = [timestamp for _, timestamp in values]
            lines.append(
                f"{name}: median={median(value for value, _ in values):g} "
                f"samples={len(values)} time_unix_nano={min(times)}..{max(times)}"
            )
    if incomplete:
        lines.append(f"incomplete measurement rows={incomplete}")
    return lines


@dataclass(slots=True)
class Outcome:
    """One test's status lines as nextest printed them."""

    binary: str
    test: str
    stress: int | None = None
    lines: list[str] = field(default_factory=list)
    failed: bool = False
    # The test's stdout and stderr blocks nextest printed under its first failed status.
    output: list[str] = field(default_factory=list)

    def names(self, case: str) -> bool:
        """Whether `case`, a nextest attempt identifier such as
        `<run>:rift::server_cli@stress-3$name`, is an attempt of this test in this
        stress iteration."""
        return names_case(case, self.binary, self.test, self.stress)

    @property
    def title(self) -> str:
        """The binary and test, and the stress iteration when nextest ran one."""
        iteration = "" if self.stress is None else f" (stress index {self.stress})"
        return f"{self.binary} {self.test}{iteration}"


def names_case(case: str, binary: str, test: str, stress: int | None = None) -> bool:
    """Whether the attempt identifier `case` names `test` of `binary`: the binary, with
    `@stress-<stress>` when a stress run adds the 0-indexed iteration, then `$` and the
    test's name."""
    head, separator, name = case.rpartition("$")
    expected = binary if stress is None else f"{binary}@stress-{stress}"
    return bool(separator) and name == test and head.endswith(expected)


def status_of(line: str) -> tuple[str, str, str, int | None] | None:
    """The status, binary, test, and 0-indexed stress iteration of a nextest status
    line (nextest prints the iteration 1-indexed as `[ 47/200]`); None for any other."""
    found = STATUS_LINE.match(line)
    if found is None:
        return None
    iteration = found["iteration"]
    stress = None if iteration is None else int(iteration) - 1
    return found["status"], found["binary"], found["test"], stress


def output_line(line: str, opened: bool) -> bool:
    """Whether `line` belongs to a test's output block nextest prints under its status
    line: a block header, or once a header `opened` the block, a blank line or a line
    indented by four spaces that is not the summary."""
    if OUTPUT_HEADER.match(line):
        return True
    return opened and (
        not line.strip() or (line.startswith("    ") and not SUMMARY_LINE.match(line))
    )


def failed_status(status: str) -> bool:
    """Whether a status ends a test with a failure."""
    return not status.startswith(PASSED)


def profile_of(arguments: Sequence[str]) -> str:
    """The nextest profile `-P` or `--profile` selects, else `NEXTEST_PROFILE`, else `default`."""
    for index, argument in enumerate(arguments):
        if argument in ("-P", "--profile") and index + 1 < len(arguments):
            return arguments[index + 1]
        if argument.startswith("--profile="):
            return argument.split("=", 1)[1]
    return os.environ.get("NEXTEST_PROFILE", "default")


def config_file_of(arguments: Sequence[str]) -> Path:
    """The nextest configuration `--config-file` names, else `.config/nextest.toml`."""
    for index, argument in enumerate(arguments):
        if argument == "--config-file" and index + 1 < len(arguments):
            return Path(arguments[index + 1])
        if argument.startswith("--config-file="):
            return Path(argument.split("=", 1)[1])
    return REPOSITORY / ".config" / "nextest.toml"


def slow_timeout(profile: str, config_file: Path) -> str:
    """The profile's `slow-timeout` from `config_file`, as text; overrides by test
    filter apply on top and are named by the file."""
    try:
        config = tomllib.loads(config_file.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        return f"unreadable: {error}"
    profiles = config.get("profile", {})
    for name in (profile, "default"):
        timeout = profiles.get(name, {}).get("slow-timeout")
        if timeout is not None:
            return f"{timeout} (profile {name}; overrides in {config_file} apply)"
    return "not set"


def cut(line: str) -> str:
    """`line` within `LINE_CHARS_MAX` characters, saying what it left out."""
    if len(line) <= LINE_CHARS_MAX:
        return line
    return f"{line[:LINE_CHARS_MAX]} [{len(line) - LINE_CHARS_MAX} characters left out]"


def newest(lines: list[str], bound: int, noun: str) -> list[str]:
    """The newest `bound` of `lines`, led by a line naming what the bound cut."""
    if len(lines) <= bound:
        return lines
    return [f"[{len(lines) - bound} older {noun} left out]", *lines[-bound:]]


def request_of(attributes: Iterable[tuple[str, str]]) -> str:
    """The `request_id` an item carries, as `req=<id>  `, or empty."""
    request = dict(attributes).get(SPAN_REQUEST_KEY, "")
    return f"req={request}  " if request else ""


def pid_of(resource: Iterable[tuple[str, str]]) -> str:
    """The sender's `process.pid`, or `?` when its resource carries none."""
    return dict(resource).get(PID_KEY, "?")


def timeline(telemetry: CaseTelemetry) -> list[str]:
    """Every log record, span begin and end, and metric point, by time."""
    entries: list[tuple[int, str]] = []
    for entry in telemetry.logs:
        entries.append(
            (
                entry.time_unix_nano,
                f"{stamp(entry.time_unix_nano)} pid={pid_of(entry.resource)}  "
                f"log {entry.severity:<5} {entry.body}   "
                + request_of(entry.attributes)
                + fields_text(entry.attributes)
                + (f"  span={entry.span_id}" if entry.span_id else ""),
            )
        )
    for span in telemetry.spans:
        context = request_of(span.attributes)
        entries.append(
            (
                span.start_time_unix_nano,
                (
                    f"{stamp(span.start_time_unix_nano)} pid={pid_of(span.resource)}  "
                    f"span begin {span.name}   {context}span={span.span_id}"
                ),
            )
        )
        entries.append((span.end_time_unix_nano, f"{span.line()}  span={span.span_id}"))
    for point in telemetry.points:
        entries.append(
            (point.time_unix_nano, f"pid={pid_of(point.resource)}  {point.line()}")
        )
    entries.sort(key=lambda entry: entry[0])
    return [cut(line) for _, line in entries]


def last_values(points: Iterable[MetricPoint]) -> list[str]:
    """The newest point of each series, per resource, start time, scope, and attributes."""
    latest: dict[tuple[str, Attributes, int, Scope, Attributes], MetricPoint] = {}
    for point in points:
        key = (
            point.name,
            point.resource,
            point.start_time_unix_nano,
            point.scope,
            point.attributes,
        )
        held = latest.get(key)
        if held is None or held.time_unix_nano <= point.time_unix_nano:
            latest[key] = point
    return [
        cut(f"pid={pid_of(point.resource)}  {point.line()}")
        for point in sorted(
            latest.values(),
            key=lambda point: (
                point.name,
                point.resource,
                point.start_time_unix_nano,
                point.scope,
                point.attributes,
            ),
        )
    ]


def still_open(logs: Iterable[LogEntry], spans: Iterable[SpanRecord]) -> list[str]:
    """Operations whose `operation opened` record arrived and whose span never ended,
    then the newest `operations in flight` record."""
    ended = {span.span_id for span in spans}
    lines: list[str] = []
    flight: LogEntry | None = None
    for entry in logs:
        if (
            entry.body == OPENED_MESSAGE
            and entry.span_id
            and entry.span_id not in ended
        ):
            lines.append(cut(entry.line()))
        if entry.body == IN_FLIGHT_MESSAGE and (
            flight is None or flight.time_unix_nano <= entry.time_unix_nano
        ):
            flight = entry
    lines = newest(lines, OPEN_LINES_MAX, "open operations")
    lines.append(
        cut(f"newest operations in flight record: {flight.line()}")
        if flight is not None
        else "newest operations in flight record: none received"
    )
    return lines


@dataclass(slots=True)
class Window:
    """What the Rust harness wrote for one test into the failure window directory."""

    path: Path
    keys: dict[str, list[str]]

    @classmethod
    def read(cls, path: Path) -> Window:
        keys: dict[str, list[str]] = {}
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            key, _, value = line.partition("=")
            keys.setdefault(key, []).append(value)
        return cls(path, keys)

    def first(self, key: str) -> str:
        return (self.keys.get(key) or [""])[0]


def windows_of(directory: Path, outcome: Outcome, since: float = 0.0) -> list[Window]:
    """The window files written since `since`, in seconds of the epoch, whose `attempt=`
    line names an attempt of `outcome`'s test. A failed window stays in the directory
    after its run, so a file older than this run belongs to an earlier one."""
    if not directory.is_dir():
        return []
    found = []
    for path in sorted(directory.glob("*.window")):
        try:
            if path.stat().st_mtime < since:
                continue
            window = Window.read(path)
        except OSError:
            continue
        if outcome.names(window.first("attempt")):
            found.append(window)
    return found


def tail(path: Path, bound: int = STDERR_BYTES_MAX) -> str:
    """The last `bound` bytes of `path`, saying what the bound cut."""
    try:
        with path.open("rb") as file:
            size = file.seek(0, os.SEEK_END)
            file.seek(max(0, size - bound))
            text = file.read().decode("utf-8", errors="replace")
    except OSError as error:
        return f"(unreadable: {error})"
    return (f"[{size - bound} earlier bytes left out]\n" if size > bound else "") + text


def processes(windows: Sequence[Window]) -> list[str]:
    """Each registered process, its exit status, and its stderr tail."""
    if not windows:
        return [
            (
                "no failure window file names this test: the test opened no "
                "`harness::FailureWindow`, or it passed its window before it failed"
            )
        ]
    lines: list[str] = []
    for window in windows:
        lines.append(f"window file: {window.path}")
        for key in ("started_at", "ended_at", "root"):
            for value in window.keys.get(key, []):
                lines.append(f"{key}: {value}")
        exits: dict[str, str] = {}
        for value in window.keys.get("exit", []):
            pid, _, status = value.partition(" ")
            exits[pid] = status
        registered = window.keys.get("process", [])
        if not registered:
            lines.append("no process registered by the harness")
        for value in registered:
            pid, _, label = value.partition(" ")
            status = exits.get(pid, "still running when the window ended")
            lines.append(f"process pid={pid} {label}: exit {status}")
        stem = window.path.name.removesuffix(".window")
        stderr_files = sorted(window.path.parent.glob(f"{stem}.*.stderr"))
        if not stderr_files:
            lines.append("no stderr file retained for this window")
        for stderr in stderr_files:
            lines.append(f"---- {stderr.name} ----")
            lines.append(tail(stderr))
        text = window.path.with_suffix(".text")
        if text.is_file():
            lines.append(f"---- {text.name}: the window the test printed ----")
            lines.append(tail(text))
    return lines


def case_report(
    outcome: Outcome,
    telemetry: Sequence[tuple[str, CaseTelemetry]],
    *,
    command: str,
    profile: str,
    config_file: Path,
    windows: Sequence[Window],
    served: Collector,
    requests: RequestSnapshot,
    request_details: bool = True,
) -> str:
    """One failed test's report, top to bottom."""
    runner = " ".join(
        f"{name}={os.environ[name]}"
        for name in (
            "RUNNER_NAME",
            "RUNNER_OS",
            "RUNNER_ARCH",
            "ImageOS",
            "ImageVersion",
        )
        if name in os.environ
    )
    sections = [
        f"==== failed test: {outcome.title} ====",
        f"os: {platform.platform()} {platform.machine()}"
        + (f"  runner: {runner}" if runner else ""),
        f"command: {command}",
        f"nextest profile {profile}, slow-timeout {slow_timeout(profile, config_file)}",
        "status lines:",
        *outcome.lines,
    ]
    logs: list[LogEntry] = []
    spans: list[SpanRecord] = []
    points: list[MetricPoint] = []
    for name, held in telemetry:
        logs.extend(held.logs)
        spans.extend(held.spans)
        points.extend(held.points)
        sections.append(
            f"telemetry of test.case.name={name}: {len(held.logs)} log records, "
            f"{len(held.spans)} spans, {len(held.points)} points; this test's bounds "
            f"dropped {held.dropped.counts()}"
        )
    if not telemetry:
        sections.append(
            "no telemetry carries this test's test.case.name: its processes exported "
            "nothing, or the test spawned no process through the harness"
        )
    merged = CaseTelemetry()
    merged.logs.extend(sorted(logs, key=lambda entry: entry.time_unix_nano))
    merged.spans.extend(spans)
    merged.points.extend(points)
    sections.append("---- timeline ----")
    sections.extend(newest(timeline(merged), TIMELINE_LINES_MAX, "timeline lines"))
    sections.append("---- last value of every instrument series ----")
    sections.extend(
        newest(last_values(points), INSTRUMENT_LINES_MAX, "series")
        or ["no metric points"]
    )
    sections.append("---- operations opened with no end received ----")
    sections.extend(still_open(logs, spans))
    sections.append("---- test output ----")
    sections.extend(
        newest([cut(line) for line in outcome.output], OUTPUT_LINES_MAX, "output lines")
        or ["nextest printed no output under this test's status"]
    )
    sections.append("---- processes ----")
    sections.extend(processes(windows))
    cases = served.cases
    sections.append(
        "---- collector ----\n"
        f"received: {served.logs.received} log records, {served.spans.received} spans, "
        f"{served.metrics.received} points; dropped: {served.dropped().counts()}; "
        + (
            f"unattributed items: {cases.unattributed}; tests refused: {cases.refused}"
            if cases is not None
            else "no per-test store"
        )
    )
    sections.append(
        "---- OTLP requests ----\n"
        f"received={requests.received} retained={requests.retained} "
        f"dropped={requests.dropped} omitted={requests.omitted}"
    )
    if request_details:
        for request in requests.requests:
            identities = (
                "; ".join(
                    " ".join(
                        f"{key}={value}"
                        for key, value in (
                            ("test.case.name", identity.test_case),
                            ("process.pid", identity.pid),
                            ("service.instance.id", identity.instance),
                        )
                        if value is not None
                    )
                    + (" truncated=true" if identity.truncated else "")
                    for identity in request.identities
                )
                or "no decoded resource identity"
            )
            sections.append(
                f"request={request.request_id} path={request.path} "
                f"intended_status={request.intended_status} outcome={request.outcome} "
                f"bytes={request.encoded_bytes}/{request.decoded_bytes} "
                f"identities_omitted={request.identities_omitted} identity={identities}"
            )
            sections.append(
                "request timestamps unix_nano "
                f"arrived={request.arrived_unix_nano} "
                f"body_read={request.body_read_unix_nano} "
                f"body_decoded={request.body_decoded_unix_nano} "
                f"protobuf_decoded={request.decoded_unix_nano} "
                f"stored={request.stored_unix_nano} "
                f"handler_finished={request.handler_finished_unix_nano}"
            )
    else:
        for request in requests.requests[-8:]:

            def elapsed(start: int | None, end: int | None) -> str:
                return (
                    "?"
                    if start is None or end is None
                    else f"{(end - start) / 1e6:.3f}"
                )

            sections.append(
                f"request={request.request_id} path={request.path} "
                f"intended_status={request.intended_status} "
                f"bytes={request.encoded_bytes}/{request.decoded_bytes} "
                f"arrival_body_ms={elapsed(request.arrived_unix_nano, request.body_read_unix_nano)} "
                f"inflate_ms={elapsed(request.body_read_unix_nano, request.body_decoded_unix_nano)} "
                f"store_ms={elapsed(request.decoded_unix_nano, request.stored_unix_nano)} "
                f"handler_ms={elapsed(request.arrived_unix_nano, request.handler_finished_unix_nano)}"
            )
    sections.append(f"==== end of failed test: {outcome.title} ====")
    return "\n".join(sections) + "\n"


def echo(data: bytes) -> None:
    """Writes `data` to stdout as bytes: nextest prints UTF-8, which a Windows console
    code page such as cp1252 cannot encode as text."""
    sys.stdout.flush()
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()


def report_name(outcome: Outcome) -> str:
    """The report's file name: the binary and test with path separators replaced."""
    iteration = "" if outcome.stress is None else f"-stress-{outcome.stress}"
    name = f"{outcome.binary}-{outcome.test}{iteration}"
    return re.sub(r"[^A-Za-z0-9._-]", "_", name) + ".txt"


def run(command: Command, arguments: Sequence[str] | None = None) -> None:
    """Runs `command`, a nextest invocation, beside a collector; reports each failure.

    Raises `CommandFailed` with nextest's status when it fails, after the reports.
    """
    shown = " ".join([command.program, *command.arguments])
    arguments_seen = list(arguments if arguments is not None else command.arguments)
    profile = profile_of(arguments_seen)
    progress_started = start("tests")
    started = time.time()
    cases = CaseStore()
    outcomes: dict[str, Outcome] = {}
    report_directory = Path(os.environ.get(REPORT_DIRECTORY_ENV, REPORT_DIRECTORY))
    if not report_directory.is_absolute():
        report_directory = REPOSITORY / report_directory
    log_path = report_directory / f"nextest-{profile}-{time.time_ns()}.log"
    evidence_path = log_path.with_suffix(".telemetry.txt")
    raw_evidence_path = log_path.with_suffix(".telemetry.jsonl")
    report_directory.mkdir(parents=True, exist_ok=True)
    log_kept = 0
    log_dropped = 0
    evidence_kept = 0
    evidence_dropped = 0
    raw_evidence_kept = 0
    raw_evidence_dropped = 0
    failed_request_ids: set[int] = set()
    pending = bytearray()
    reading: Outcome | None = None
    opened = False
    status: int | None = None
    stream_error: BaseException | None = None

    def write_raw_evidence(name: str, held: CaseTelemetry) -> None:
        nonlocal raw_evidence_kept, raw_evidence_dropped

        raw_records = [
            *(("log record", entry) for entry in held.logs),
            *(("span", span) for span in held.spans),
            *(("metric point", point) for point in held.points),
        ]
        if raw_records:
            with raw_evidence_path.open("ab") as artifact:
                for kind, record in raw_records:
                    remaining = (
                        CASE_EVIDENCE_BYTES_MAX - evidence_kept - raw_evidence_kept
                    )
                    if remaining <= 0:
                        raw_evidence_dropped += 1
                        continue
                    data = (
                        json.dumps(
                            {
                                "test.case.name": name,
                                "kind": kind,
                                "record": asdict(record),
                            },
                            separators=(",", ":"),
                        )
                        + "\n"
                    ).encode("utf-8")
                    if len(data) > remaining:
                        raw_evidence_dropped += 1
                        continue
                    artifact.write(data)
                    raw_evidence_kept += len(data)

    def write_request_evidence(requests: RequestSnapshot) -> None:
        nonlocal raw_evidence_kept, raw_evidence_dropped
        if not requests.requests:
            return
        ordered = sorted(
            requests.requests,
            key=lambda request: request.request_id not in failed_request_ids,
        )
        with raw_evidence_path.open("ab") as artifact:
            for index, request in enumerate(ordered):
                remaining = CASE_EVIDENCE_BYTES_MAX - evidence_kept - raw_evidence_kept
                if remaining <= 0:
                    raw_evidence_dropped += len(ordered) - index
                    return
                data = (
                    json.dumps(
                        {"kind": "collector request", "record": asdict(request)},
                        separators=(",", ":"),
                    )
                    + "\n"
                ).encode("utf-8")
                if len(data) > remaining:
                    raw_evidence_dropped += 1
                    continue
                artifact.write(data)
                raw_evidence_kept += len(data)

    def capture_passing(outcome: Outcome) -> None:
        nonlocal evidence_kept, evidence_dropped
        for name in cases.matching(outcome.names):
            held = cases.take(name)
            if held is None:
                continue
            measurements = measurement_summary(held.logs)
            evidence = [
                f"test.case.name={name}",
                f"logs={len(held.logs)} spans={len(held.spans)} points={len(held.points)}",
                f"dropped={held.dropped.counts()}",
                *measurements,
                "---- log records ----",
                *newest([entry.line() for entry in held.logs], 3, "log records"),
                "---- spans ----",
                *newest([span.line() for span in held.spans], 3, "spans"),
                "---- last metric value of each series ----",
                *newest(last_values(held.points), 12, "metric series"),
                "---- timeline ----",
                *newest(timeline(held), CASE_EVIDENCE_LINES_MAX, "timeline lines"),
                "",
            ]
            data = ("\n".join(evidence)).encode("utf-8")
            remaining = max(0, CASE_EVIDENCE_BYTES_MAX - evidence_kept)
            retained = data[:remaining]
            if retained:
                with evidence_path.open("ab") as artifact:
                    artifact.write(retained)
            evidence_kept += len(retained)
            evidence_dropped += len(data) - len(retained)
            write_raw_evidence(name, held)

    def line(raw: bytes) -> None:
        nonlocal reading, opened
        text = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        found = status_of(text)
        if found is None:
            if reading is not None and output_line(text, opened):
                opened = True
                reading.output.append(text)
                if len(reading.output) > 2 * OUTPUT_LINES_MAX:
                    del reading.output[:OUTPUT_LINES_MAX]
            else:
                reading = None
            return
        test_status, binary, test, stress = found
        key = f"{binary}${test}@{stress}"
        outcome = outcomes.setdefault(
            key, Outcome(binary=binary, test=test, stress=stress)
        )
        if text.strip() not in outcome.lines:
            outcome.lines.append(text.strip())
        reading, opened = None, False
        if failed_status(test_status):
            outcome.failed = True
            echo(f"{text.strip()}\n".encode())
            if not outcome.output:
                reading = outcome
        elif not outcome.failed:
            capture_passing(outcome)
            cases.forget(outcome.names)
            del outcomes[key]

    def on_bytes(data: bytes) -> None:
        nonlocal log_kept, log_dropped
        remaining = max(0, RUN_LOG_BYTES_MAX - log_kept)
        retained = data[:remaining]
        if retained:
            with log_path.open("ab") as log:
                log.write(retained)
            log_kept += len(retained)
        log_dropped += len(data) - len(retained)
        pending.extend(data)
        while (newline := pending.find(b"\n")) >= 0:
            current = bytes(pending[: newline + 1])
            del pending[: newline + 1]
            line(current)

    with collector(cases=cases) as served:
        # Keep an explicit caller choice. The default disables in-process exporters
        # during the partial Rust migration; child processes still receive collector
        # settings and the harness removes this variable from their environment.
        inherited = command.environment()
        if inherited is None:
            inherited = os.environ
        sdk_disabled = inherited.get(SDK_DISABLED, "true")
        command.with_env(
            **served.environment(source=inherited),
            **{SDK_DISABLED: sdk_disabled, RECORDER_STREAM: "1"},
        )
        try:
            completion = asyncio.run(
                command.stream(on_bytes, exit_wait_seconds=EXIT_WAIT_SECONDS)
            )
            status = completion.status
        except BaseException as error:  # noqa: BLE001 - report evidence before re-raising.
            stream_error = error
            if isinstance(error, CommandFailed):
                status = error.status
        if pending:
            line(bytes(pending))
            pending.clear()
        failed = [outcome for outcome in outcomes.values() if outcome.failed]
        directory = REPOSITORY / WINDOW_DIRECTORY.format(profile=profile)
        for outcome in failed:
            telemetry = [
                (name, held)
                for name in cases.matching(outcome.names)
                if (held := cases.take(name)) is not None
            ]
            for name, held in telemetry:
                write_raw_evidence(name, held)
            windows = windows_of(directory, outcome, started)
            process_ids = {
                window_pid
                for window in windows
                for value in window.keys.get("process", [])
                if (window_pid := value.partition(" ")[0])
            }
            requests = served.requests.for_failure(
                cases.matching(outcome.names), process_ids
            )
            failed_request_ids.update(
                request.request_id for request in requests.requests
            )
            oldest_retained = max(0, requests.received - EXPORT_REQUESTS_MAX)
            failed_request_ids.intersection_update(
                {
                    request_id
                    for request_id in failed_request_ids
                    if request_id > oldest_retained
                }
            )
            report = case_report(
                outcome,
                telemetry,
                command=shown,
                profile=profile,
                config_file=config_file_of(arguments_seen),
                windows=windows,
                served=served,
                requests=requests,
            )
            console_report = case_report(
                outcome,
                telemetry,
                command=shown,
                profile=profile,
                config_file=config_file_of(arguments_seen),
                windows=windows,
                served=served,
                requests=requests,
                request_details=False,
            )
            path = report_directory / report_name(outcome)
            path.write_text(report, encoding="utf-8")
            echo(console_report.encode("utf-8"))
            echo(f"[report written to {path}]\n".encode())
        write_request_evidence(served.requests.for_run(failed_request_ids))
    failed = (
        stream_error is not None
        or status != 0
        or any(outcome.failed for outcome in outcomes.values())
    )
    finish("tests", progress_started, failed=failed)
    if log_dropped:
        echo(f"[run log left out {log_dropped} bytes: {log_path}]\n".encode())
    if evidence_kept:
        echo(f"[telemetry evidence written to {evidence_path}]\n".encode())
    if evidence_dropped:
        echo(
            f"[telemetry evidence left out {evidence_dropped} bytes: {evidence_path}]\n".encode()
        )
    if raw_evidence_kept:
        echo(f"[raw telemetry written to {raw_evidence_path}]\n".encode())
    if raw_evidence_dropped:
        echo(
            f"[raw telemetry left out {raw_evidence_dropped} records: "
            f"{raw_evidence_path}]\n".encode()
        )
    if stream_error is not None:
        raise stream_error.with_traceback(stream_error.__traceback__)
    if status != 0:
        raise CommandFailed(command, status or 0, "")
