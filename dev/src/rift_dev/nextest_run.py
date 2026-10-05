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
- each process the harness registered: its exit status and the tail of its stderr,
  from the failure window directory `target/nextest/<profile>/failure-windows/`;
- what the collector received, dropped, and could not attribute.

Each part prints at most its named bound and says what the bound cut.
"""

from __future__ import annotations

import os
import platform
import re
import sys
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from pathlib import Path

import tomllib

from rift_dev.commands import REPOSITORY, Command, CommandFailed
from rift_dev.trace import (
    PID_KEY,
    SPAN_REQUEST_KEY,
    CaseStore,
    CaseTelemetry,
    Collector,
    LogEntry,
    MetricPoint,
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
# Statuses that end a test without a failure.
PASSED = ("PASS", "LEAK", "FLAKY")
# The report directory below the repository, which CI uploads.
REPORT_DIRECTORY = REPOSITORY / "target" / "integration" / "nextest"
# The failure window directory of one profile, below the repository (`harness.rs`).
WINDOW_DIRECTORY = "target/nextest/{profile}/failure-windows"
# Lines one report prints per part, the newest of each.
TIMELINE_LINES_MAX = 3_000
INSTRUMENT_LINES_MAX = 400
OPEN_LINES_MAX = 100
STDERR_BYTES_MAX = 64 * 1024
# How long the runner waits for nextest to exit once its output closed.
EXIT_WAIT_SECONDS = 60.0
# Characters one report line keeps.
LINE_CHARS_MAX = 2_000
# The log record message an operation declared with `open = true` opens with
# (`crates/rift-tracing/src/span.rs`), and the table of operations in flight's.
OPENED_MESSAGE = "operation opened"
IN_FLIGHT_MESSAGE = "operations in flight"


@dataclass(slots=True)
class Outcome:
    """One test's status lines as nextest printed them."""

    binary: str
    test: str
    stress: int | None = None
    lines: list[str] = field(default_factory=list)
    failed: bool = False

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


def failed_status(status: str) -> bool:
    """Whether a status ends a test with a failure."""
    return not status.startswith(PASSED)


def profile_of(arguments: Sequence[str]) -> str:
    """The nextest profile `--profile` selects, else `NEXTEST_PROFILE`, else `default`."""
    for index, argument in enumerate(arguments):
        if argument == "--profile" and index + 1 < len(arguments):
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
    """The newest point of every instrument series, per process."""
    latest: dict[tuple[str, str, tuple[tuple[str, str], ...]], MetricPoint] = {}
    for point in points:
        key = (point.name, pid_of(point.resource), point.attributes)
        held = latest.get(key)
        if held is None or held.time_unix_nano <= point.time_unix_nano:
            latest[key] = point
    return [
        cut(f"pid={pid_of(point.resource)}  {point.line()}")
        for point in sorted(latest.values(), key=lambda point: point.name)
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


def windows_of(directory: Path, outcome: Outcome) -> list[Window]:
    """The window files whose `attempt=` line names an attempt of `outcome`'s test."""
    if not directory.is_dir():
        return []
    found = []
    for path in sorted(directory.glob("*.window")):
        try:
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
    cases = CaseStore()
    outcomes: dict[str, Outcome] = {}
    with collector(cases=cases) as served:
        # A test process that installs Rift's tracing itself must not export: its
        # in-process export would differ from the runs the test was written for. The
        # harness removes the variable from every `rift` process it spawns.
        command.with_env(**served.environment(), **{SDK_DISABLED: "true"})
        with command.spawn() as process:
            assert process.stdout is not None
            for raw in process.stdout:
                line = raw.decode("utf-8", errors="replace")
                echo(raw)
                found = status_of(line.rstrip("\n"))
                if found is None:
                    continue
                status, binary, test, stress = found
                outcome = outcomes.setdefault(
                    f"{binary}${test}@{stress}",
                    Outcome(binary=binary, test=test, stress=stress),
                )
                if line.strip() not in outcome.lines:
                    outcome.lines.append(line.strip())
                if failed_status(status):
                    outcome.failed = True
                elif not outcome.failed:
                    cases.forget(outcome.names)
                    del outcomes[f"{binary}${test}@{stress}"]
            status = process.wait(EXIT_WAIT_SECONDS)
        sys.stdout.flush()
        failed = [outcome for outcome in outcomes.values() if outcome.failed]
        directory = REPOSITORY / WINDOW_DIRECTORY.format(profile=profile)
        if failed:
            REPORT_DIRECTORY.mkdir(parents=True, exist_ok=True)
        for outcome in failed:
            telemetry = [
                (name, held)
                for name in cases.matching(outcome.names)
                if (held := cases.take(name)) is not None
            ]
            report = case_report(
                outcome,
                telemetry,
                command=shown,
                profile=profile,
                config_file=config_file_of(arguments_seen),
                windows=windows_of(directory, outcome),
                served=served,
            )
            path = REPORT_DIRECTORY / report_name(outcome)
            path.write_text(report, encoding="utf-8")
            echo(report.encode("utf-8"))
            echo(f"[report written to {path}]\n".encode())
        sys.stdout.flush()
    if status != 0:
        raise CommandFailed(command, status, "")
