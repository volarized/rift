"""Collect Nextest results and OTLP records under exact invocation identities."""

from __future__ import annotations

import asyncio
import math
import os
import platform
import re
import threading
import time
import uuid
from collections.abc import Iterable, Iterator, Sequence
from contextlib import contextmanager
from pathlib import Path
from statistics import median
from typing import Annotated, Literal

import tomllib
from pydantic import BaseModel, ConfigDict, Field, TypeAdapter, ValidationError

from rift_dev.commands import REPOSITORY, CargoCommand, Command, CommandFailed
from rift_dev.progress import start
from rift_dev.trace import (
    EXPORT_REQUEST_REPORT_MAX,
    PID_KEY,
    SPAN_REQUEST_KEY,
    Attributes,
    CaseStore,
    CaseTelemetry,
    Collector,
    Dropped,
    ExportRequest,
    LogEntry,
    MetricPoint,
    Scope,
    SpanRecord,
    collector,
    fields_text,
    stamp,
)

REPORT_DIRECTORY = REPOSITORY / "target/integration/nextest"
REPORT_DIRECTORY_ENV = "RIFT_TEST_REPORT_DIRECTORY"
RUN_LOG_BYTES_MAX = 16 * 1024 * 1024
# Original records for a full workspace run require more than 16 MiB.
# This bounds disk retention independently of the diagnostic caches.
RUN_TELEMETRY_BYTES_MAX = 256 * 1024 * 1024
EVENT_BYTES_MAX = 16 * 1024 * 1024
CASES_MAX = 32_768
RUN_SECONDS_MAX = 3_600.0
EXIT_WAIT_SECONDS = 10.0
CONSOLE_BYTES_MAX = 64 * 1024
CONSOLE_EVIDENCE_MAX = 8
LINE_CHARS_MAX = 2_000
OPEN_LINES_MAX = 100
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


class NextestMetadata(BaseModel):
    """Public JSON+ metadata emitted by pinned Nextest."""

    crate: str
    test_binary: str
    kind: str
    stress_index: int | None = None


class TestEvent(BaseModel):
    """One public JSON+ test event; output is Nextest's combined capture."""

    model_config = ConfigDict(strict=True)
    type: Literal["test"]
    event: Literal["started", "ok", "failed", "ignored"]
    name: str
    exec_time: float | None = None
    stdout: str = ""
    reason: str = ""


class SuiteEvent(BaseModel):
    """One public JSON+ suite event."""

    model_config = ConfigDict(strict=True)
    type: Literal["suite"]
    event: Literal["started", "ok", "failed"]
    nextest: NextestMetadata


EVENT = TypeAdapter(Annotated[TestEvent | SuiteEvent, Field(discriminator="type")])


class FilterMatch(BaseModel):
    """Nextest discovery's selected or filtered case."""

    status: Literal["matches", "mismatch"]


class ListedTest(BaseModel):
    """One discovery entry, including ignored selection."""

    filter_match: FilterMatch = Field(alias="filter-match")


class ListedSuite(BaseModel):
    """Discovery owns binary IDs; JSON+ owns display names."""

    package_name: str = Field(alias="package-name")
    binary_id: str = Field(alias="binary-id")
    binary_name: str = Field(alias="binary-name")
    testcases: dict[str, ListedTest]


class TestList(BaseModel):
    """Nextest's public test list, validated before execution."""

    rust_suites: dict[str, ListedSuite] = Field(alias="rust-suites")


class CaseIdentity(BaseModel):
    """Exact discovery identity for one selected test."""

    binary: str
    test: str
    full_name: str
    selected: bool


class AttemptIdentity(BaseModel):
    """Identity read from an emitting process, never decoded from its attempt ID."""

    run_id: uuid.UUID = Field(alias="nextest.run_id")
    binary: str = Field(alias="nextest.binary_id")
    test: str = Field(alias="nextest.test_name")
    attempt: Literal["1"] = Field(alias="nextest.attempt")
    total_attempts: Literal["1"] = Field(alias="nextest.total_attempts")
    attempt_id: str = Field(alias="test.case.name")


class Outcome(BaseModel):
    """Started and terminal result for one selected case."""

    case: CaseIdentity
    started: bool = False
    result: TestEvent | None = None

    @property
    def failed(self) -> bool:
        return self.started and (self.result is None or self.result.event != "ok")


class LogEvidence(BaseModel):
    kind: Literal["log record"] = "log record"
    record: LogEntry


class SpanEvidence(BaseModel):
    kind: Literal["span"] = "span"
    record: SpanRecord


class MetricEvidence(BaseModel):
    kind: Literal["metric point"] = "metric point"
    record: MetricPoint


class RequestEvidence(BaseModel):
    kind: Literal["collector request"] = "collector request"
    record: ExportRequest


EVIDENCE = TypeAdapter(
    Annotated[
        LogEvidence | SpanEvidence | MetricEvidence | RequestEvidence,
        Field(discriminator="kind"),
    ]
)


class CollectionSummary(BaseModel):
    """Original records, retained artifacts, and diagnostic cache accounting."""

    received: int
    retained: int
    omitted: int
    received_bytes: int
    errors: tuple[str, ...] = ()
    unassigned: int = 0
    cache_evictions: Dropped = Field(default_factory=Dropped)
    request_cache_evictions: int = 0
    log_bytes_omitted: int = 0


class RunSummary(CollectionSummary):
    """Execution and collection remain separate results."""

    invocation: str
    command: str
    platform: str
    profile: str
    status: int | None
    elapsed_seconds: float
    launched: int
    attempts_launched: int
    succeeded: int
    failed: int
    incomplete: int
    unlaunched: int
    attempts: tuple[AttemptIdentity, ...]
    artifacts: tuple[str, ...]


@contextmanager
def retained_collector(directory: Path) -> Iterator[Collector]:
    """Keep scenario telemetry through server cleanup and collector shutdown."""
    directory.mkdir(parents=True, exist_ok=True)
    artifact = ArtifactStore(directory / "telemetry.jsonl", {})
    served = Collector()
    try:
        with collector(
            cases=CaseStore(observe=artifact.record_only),
            request_observer=artifact.request,
        ) as served:
            yield served
    finally:
        summary = CollectionSummary(
            errors=tuple(
                [
                    f"collection omitted={artifact.omitted} requests_failed={artifact.request_errors}"
                ]
                if artifact.omitted or artifact.request_errors
                else []
            ),
            received=artifact.received,
            retained=artifact.retained,
            omitted=artifact.omitted,
            received_bytes=artifact.received_bytes,
            unassigned=0,
            cache_evictions=served.dropped(),
            request_cache_evictions=served.requests.dropped,
            log_bytes_omitted=0,
        )
        (directory / "telemetry-summary.json").write_text(
            summary.model_dump_json(indent=2) + "\n", encoding="utf-8"
        )
    if artifact.omitted or artifact.request_errors:
        raise RuntimeError(
            f"collection incomplete; inspect {directory / 'telemetry-summary.json'}"
        )


class ArtifactStore:
    """Spool original records while bounded stores serve live test snapshots."""

    def __init__(self, path: Path, outcomes: dict[str, Outcome]) -> None:
        self.path = path
        self.outcomes = outcomes
        self.cases = {(o.case.binary, o.case.test): o for o in outcomes.values()}
        self.identities: dict[str, AttemptIdentity] = {}
        self.run_id: uuid.UUID | None = None
        self.received = 0
        self.retained = 0
        self.omitted = 0
        self.unassigned = 0
        self.request_errors = 0
        self.write_error: str | None = None
        self.written = 0
        self.received_bytes = 0
        self.lock = threading.Lock()
        self.path.touch()

    def write(
        self, evidence: LogEvidence | SpanEvidence | MetricEvidence | RequestEvidence
    ) -> None:
        data = EVIDENCE.dump_json(evidence) + b"\n"
        with self.lock:
            self.received += 1
            self.received_bytes += len(data)
            if self.written + len(data) > RUN_TELEMETRY_BYTES_MAX:
                self.omitted += 1
                return
            try:
                with self.path.open("ab") as artifact:
                    artifact.write(data)
            except OSError as error:
                self.write_error = str(error)[:LINE_CHARS_MAX]
                self.omitted += 1
                return
            self.written += len(data)
            self.retained += 1

    def observe(self, record: LogEntry | SpanRecord | MetricPoint) -> bool:
        self.record(record)
        try:
            identity = AttemptIdentity.model_validate(dict(record.resource))
        except ValidationError:
            self.unassigned += 1
            return False
        outcome = self.cases.get((identity.binary, identity.test))
        if outcome is None or (
            self.run_id is not None and self.run_id != identity.run_id
        ):
            self.unassigned += 1
            return False
        self.run_id = identity.run_id
        held = self.identities.get(identity.attempt_id)
        if held is not None and held != identity:
            self.unassigned += 1
            return False
        if held is None and len(self.identities) >= CASES_MAX:
            self.unassigned += 1
            return False
        self.identities[identity.attempt_id] = identity
        # Original records remain in JSONL after terminal results and late arrivals.
        return outcome.result is None or outcome.failed

    def record(self, record: LogEntry | SpanRecord | MetricPoint) -> None:
        """Serialize one original telemetry record through its Pydantic model."""
        if isinstance(record, LogEntry):
            self.write(LogEvidence(record=record))
        elif isinstance(record, SpanRecord):
            self.write(SpanEvidence(record=record))
        else:
            self.write(MetricEvidence(record=record))

    def record_only(self, record: LogEntry | SpanRecord | MetricPoint) -> bool:
        """Persist suite records without retaining another per-case copy."""
        self.record(record)
        return False

    def request(self, request: ExportRequest) -> None:
        """Retain every finished request before its diagnostic cache expires."""
        self.write(RequestEvidence(record=request))
        if request.intended_status != 200 or request.outcome != "ok":
            self.request_errors += 1

    def telemetry(self, outcomes: Sequence[Outcome]) -> dict[str, CaseTelemetry]:
        selected = {(o.case.binary, o.case.test): o.case.full_name for o in outcomes}
        attempts = {
            name: selected[(identity.binary, identity.test)]
            for name, identity in self.identities.items()
            if (identity.binary, identity.test) in selected
        }
        held = {o.case.full_name: CaseTelemetry() for o in outcomes}
        with self.path.open("rb") as source:
            for line in source:
                row = EVIDENCE.validate_json(line)
                if isinstance(row, RequestEvidence):
                    continue
                attempt = dict(row.record.resource).get("test.case.name", "")
                full_name = attempts.get(attempt)
                if full_name is None:
                    continue
                telemetry = held[full_name]
                if isinstance(row, LogEvidence):
                    if len(telemetry.logs) == telemetry.logs.maxlen:
                        telemetry.dropped.logs += 1
                    telemetry.logs.append(row.record)
                elif isinstance(row, SpanEvidence):
                    if len(telemetry.spans) == telemetry.spans.maxlen:
                        telemetry.dropped.spans += 1
                    telemetry.spans.append(row.record)
                else:
                    if len(telemetry.points) == telemetry.points.maxlen:
                        telemetry.dropped.points += 1
                    telemetry.points.append(row.record)
        return held


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


# These options affect reporting or scheduling, not discovery. Nextest's list
# command accepts the remaining selection, build, archive, and profile options.
RUN_VALUE_OPTIONS = frozenset(
    {
        "--no-tests",
        "--retries",
        "--test-threads",
        "-j",
        "--success-output",
        "--failure-output",
        "--status-level",
        "--final-status-level",
        "--message-format",
        "--message-format-version",
        "--max-fail",
        "--max-progress-running",
    }
)
RUN_FLAG_OPTIONS = frozenset(
    {"--no-report", "--no-fail-fast", "--fail-fast", "--hide-progress-bar"}
)
UNSUPPORTED_OPTIONS = frozenset(
    {"--no-capture", "--nocapture", "--stress-count", "--stress-duration"}
)


def list_arguments(arguments: Sequence[str]) -> list[str]:
    """Keep discovery selection unchanged while removing execution-only options."""
    retained: list[str] = []
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == "--":
            retained.extend(arguments[index:])
            break
        name = argument.partition("=")[0]
        if name in UNSUPPORTED_OPTIONS:
            raise ValueError(
                f"appliance requires captured, single-attempt execution: {name}"
            )
        if name in RUN_VALUE_OPTIONS:
            index += 1 if "=" in argument else 2
            continue
        if name not in RUN_FLAG_OPTIONS:
            retained.append(argument)
        index += 1
    return retained


def append_options(command: Command, *options: str) -> None:
    """Place runner options before the emulated libtest separator."""
    index = (
        command.arguments.index("--")
        if "--" in command.arguments
        else len(command.arguments)
    )
    command.arguments[index:index] = options


def controlled_arguments(arguments: Sequence[str]) -> list[str]:
    """Own JSON+ format and zero retries without passing duplicate CLI options."""
    retained: list[str] = []
    index = 0
    controlled = {"--message-format", "--message-format-version", "--retries"}
    while index < len(arguments):
        argument = arguments[index]
        if argument == "--":
            retained.extend(arguments[index:])
            break
        name, separator, value = argument.partition("=")
        if name == "--user-config-file":
            raise ValueError("appliance owns the isolated recording user configuration")
        if name in controlled:
            if not separator:
                index += 1
                if index >= len(arguments):
                    raise ValueError(f"missing value for {name}")
                value = arguments[index]
            if name == "--retries" and value != "0":
                raise ValueError("appliance requires zero retries")
        else:
            retained.append(argument)
        index += 1
    return retained


def prepare(command: Command, directory: Path) -> tuple[dict[str, Outcome], Path]:
    """Discover exact identities using the same instrumented build as execution."""
    arguments = controlled_arguments(command.arguments[2:])
    command.arguments = [*command.arguments[:2], *arguments]
    listing = list_arguments(arguments)
    source = command.environment() or dict(os.environ)
    if source.get("CI") and source.get("OTEL_SDK_DISABLED", "").lower() == "true":
        raise ValueError("CI appliance requires OTEL_SDK_DISABLED=false")
    if source.get("OTEL_SDK_DISABLED", "").lower() == "true":
        raise ValueError("appliance collection requires OTEL_SDK_DISABLED=false")
    llvm_cov = command.arguments[:2] == ["llvm-cov", "nextest"]
    archived = any(a.partition("=")[0] == "--archive-file" for a in listing)
    archive_directory: Path | None = None
    if llvm_cov and archived:
        # cargo-llvm-cov injects --extract-to during execution. Discovery must
        # use that same directory and must not forward a duplicate option.
        target = source.get(
            "CARGO_LLVM_COV_TARGET_DIR", str(REPOSITORY / "target/llvm-cov-target")
        )
        retained: list[str] = []
        index = 0
        while index < len(arguments):
            argument = arguments[index]
            if argument == "--":
                retained.extend(arguments[index:])
                break
            name, separator, value = argument.partition("=")
            if name == "--extract-to":
                if not separator:
                    index += 1
                    if index >= len(arguments):
                        raise ValueError("missing value for --extract-to")
                    value = arguments[index]
                target = value
            else:
                retained.append(argument)
            index += 1
        archive_directory = Path(target)
        if not archive_directory.is_absolute():
            archive_directory = command.directory / archive_directory
        archive_directory.mkdir(parents=True, exist_ok=True)
        command.with_env(CARGO_LLVM_COV_TARGET_DIR=str(archive_directory))
        arguments = retained
        command.arguments = [*command.arguments[:2], *arguments]
        listing = list_arguments(arguments)
    if llvm_cov and not archived:
        # show-env is cargo-llvm-cov's supported custom-workflow interface. One
        # instrumented build supplies discovery, execution, and the later report.
        from rift_dev.suites import parse_coverage_environment

        target = source.get(
            "CARGO_LLVM_COV_TARGET_DIR", str(REPOSITORY / "target/llvm-cov-target")
        )
        coverage = (
            CargoCommand("llvm-cov", "show-env", "--sh")
            .with_environment(source)
            .with_env(CARGO_TARGET_DIR=target)
        )
        command.with_env(
            **parse_coverage_environment(coverage.output()), CARGO_TARGET_DIR=target
        )
        command.arguments = [
            "nextest",
            "run",
            *[a for a in arguments if a != "--no-report"],
        ]
    discovery = CargoCommand("nextest", "list", *listing, "--message-format", "json")
    # Options cannot follow the emulated libtest separator.
    discovery.arguments = ["nextest", "list", *listing]
    append_options(discovery, "--message-format", "json")
    if archive_directory is not None:
        append_options(discovery, "--extract-to", str(archive_directory))
    discovery.with_environment(command.environment() or source).with_cwd(
        command.directory
    )
    discovery.with_timeout(RUN_SECONDS_MAX).with_output_limit(RUN_LOG_BYTES_MAX)
    discovered = TestList.model_validate_json(discovery.output_bytes(echo_stderr=False))
    outcomes: dict[str, Outcome] = {}
    for suite in discovered.rust_suites.values():
        for name, test in suite.testcases.items():
            full_name = f"{suite.package_name}::{suite.binary_name}${name}"
            if full_name in outcomes:
                raise ValueError(f"duplicate Nextest full name: {full_name}")
            outcomes[full_name] = Outcome(
                case=CaseIdentity(
                    binary=suite.binary_id,
                    test=name,
                    full_name=full_name,
                    selected=test.filter_match.status == "matches",
                )
            )
            if len(outcomes) > CASES_MAX:
                raise ValueError(f"Nextest discovery exceeded {CASES_MAX} cases")
    if not any(outcome.case.selected for outcome in outcomes.values()):
        raise ValueError("Nextest discovered no tests")
    config = directory / "record.toml"
    config.write_text(
        "[experimental]\nrecord = true\n\n[record]\nenabled = true\n", encoding="utf-8"
    )
    # An isolated store makes `latest` belong to this invocation, including failure.
    command.with_env(
        NEXTEST_STATE_DIR=str(directory / "store"),
        NEXTEST_EXPERIMENTAL_LIBTEST_JSON="1",
        NEXTEST_RETRIES="0",
    )
    append_options(
        command,
        "--user-config-file",
        str(config),
        "--message-format",
        "libtest-json-plus",
        "--message-format-version",
        "0.1",
        "--retries",
        "0",
    )
    if command.timeout_seconds is None:
        command.with_timeout(RUN_SECONDS_MAX)
    return outcomes, config


class EventReader:
    """Bound JSONL buffering without stopping either command pipe drain."""

    def __init__(
        self, outcomes: dict[str, Outcome], cases: CaseStore, artifact: ArtifactStore
    ) -> None:
        self.outcomes = outcomes
        self.cases = cases
        self.artifact = artifact
        self.pending = bytearray()
        self.discarding = False
        self.errors: list[str] = []
        self.error_count = 0

    def error(self, detail: str) -> None:
        self.error_count += 1
        if len(self.errors) < CONSOLE_EVIDENCE_MAX:
            self.errors.append(detail)

    def feed(self, data: bytes) -> None:
        for segment in data.splitlines(keepends=True):
            ended = segment.endswith(b"\n")
            if len(self.pending) + len(segment) > EVENT_BYTES_MAX:
                self.pending.clear()
                self.discarding = True
                self.error("Nextest JSON+ event exceeded byte bound")
            if not self.discarding:
                self.pending.extend(segment)
            if ended:
                if not self.discarding:
                    self.line(bytes(self.pending))
                self.pending.clear()
                self.discarding = False

    def line(self, data: bytes) -> None:
        try:
            event = EVENT.validate_json(data)
        except ValidationError as error:
            self.error(f"invalid Nextest JSON+ event: {str(error)[:LINE_CHARS_MAX]}")
            return
        if isinstance(event, SuiteEvent):
            if event.nextest.stress_index is not None:
                self.error("Nextest stress results cannot identify individual attempts")
            return
        outcome = self.outcomes.get(event.name)
        if outcome is None:
            self.error(f"unassigned Nextest full name: {event.name}")
            return
        if event.event == "started":
            # JSON+ emits starts for ignore-mismatch skips. Discovery identifies
            # these even when Nextest omits their ignored terminal event.
            if not outcome.case.selected:
                return
            if outcome.started:
                self.error(f"duplicate Nextest start: {event.name}")
            outcome.started = True
        elif event.event == "ignored":
            outcome.started = False
            outcome.result = event
        else:
            if outcome.result is not None or not outcome.started:
                self.error(
                    f"Nextest terminal result without unique start: {event.name}"
                )
            outcome.result = event
            if event.event == "ok":
                with self.cases.lock:
                    names = [
                        name
                        for name, identity in self.artifact.identities.items()
                        if identity.binary == outcome.case.binary
                        and identity.test == outcome.case.test
                    ]
                    for name in names:
                        self.cases.tests.pop(name, None)


def failure_report(
    outcome: Outcome,
    held: CaseTelemetry,
    artifact: ArtifactStore,
    command: Command,
    profile: str,
) -> str:
    """Render original bounded records and exact attempt metadata before cleanup."""
    result = outcome.result
    identities = [
        identity
        for identity in artifact.identities.values()
        if identity.binary == outcome.case.binary and identity.test == outcome.case.test
    ]
    lines = [
        f"{outcome.case.test} failed!",
        "  Error:",
        result.stdout if result else "No terminal result received.",
        result.reason if result else "",
        f"  Binary: {outcome.case.binary}",
        f"  Full name: {outcome.case.full_name}",
        f"  Platform: {platform.platform()}",
        f"  Profile: {profile}",
        f"  Command: {command}",
        f"  Status: {result.event if result else 'incomplete'}",
        f"  Test deadline: {slow_timeout(profile, config_file_of(command.arguments))}",
        "  Attempts:",
        *[f"    {identity.model_dump_json(by_alias=True)}" for identity in identities],
        "  Output capture: JSON+ reports combined output. Portable recording retains captured output; JSON+ supplies no truncation metadata.",
        "  Logs:",
    ]
    for entry in held.logs:
        lines.extend(
            [
                f"    {entry.line()} trace={entry.trace_id} span={entry.span_id}",
                f"      resource: {fields_text(entry.resource)}",
            ]
        )
    lines.append("  Metrics:")
    for point in held.points:
        lines.extend(
            [f"    {point.line()}", f"      resource: {fields_text(point.resource)}"]
        )
    if held.spans:
        lines.append("  Traces:")
        for span in held.spans:
            lines.extend(
                [
                    f"    {span.line()} trace={span.trace_id} span={span.span_id} parent={span.parent_span_id} status={span.status_code} {span.status_message}",
                    f"      start={stamp(span.start_time_unix_nano)} end={stamp(span.end_time_unix_nano)} resource: {fields_text(span.resource)}",
                ]
            )
    lines.extend(still_open(held.logs, held.spans))
    lines.extend(measurement_summary(held.logs))
    lines.append(
        f"  Report omissions: {held.dropped.counts()}; original records: {artifact.path}"
    )
    lines.append(f"  Artifacts: {artifact.path.parent}")
    return "\n".join(lines) + "\n"


def success_line(count: int, elapsed: float) -> str:
    """Round elapsed duration up so the stated bound contains the run."""
    minutes, seconds = divmod(math.ceil(elapsed), 60)
    return f"{count} tests launched, all succeeded in under {minutes} minutes and {seconds} seconds."


def collection_report(
    directory: Path, errors: Sequence[str], served: Collector, artifact: ArtifactStore
) -> None:
    """Keep request timing and omission totals separate from assertion failures."""
    from collections import deque

    requests: deque[RequestEvidence] = deque(maxlen=EXPORT_REQUEST_REPORT_MAX)
    with artifact.path.open("rb") as source:
        for line in source:
            try:
                evidence = EVIDENCE.validate_json(line)
            except ValidationError:
                continue
            if isinstance(evidence, RequestEvidence):
                requests.append(evidence)
    lines = [
        "==== collection errors ====",
        *errors,
        f"received={artifact.received} retained={artifact.retained} omitted={artifact.omitted} unassigned={artifact.unassigned}",
        f"diagnostic cache evictions: {served.dropped().counts()}",
        f"requests received={served.requests.received}; newest retained timing rows below, at most {EXPORT_REQUEST_REPORT_MAX}",
        "handler_finished is recorded before returning the HTTP response; intended_status is not client receipt",
        *[request.model_dump_json() for request in requests],
        f"original records: {artifact.path}",
    ]
    (directory / "collection-error.txt").write_text(
        "\n".join(lines) + "\n", encoding="utf-8"
    )


def run(command: Command, arguments: Sequence[str] | None = None) -> None:
    """Run one captured Nextest invocation, saving evidence before raising failures."""
    arguments_seen = list(arguments if arguments is not None else command.arguments)
    profile = profile_of(arguments_seen)
    root = Path(os.environ.get(REPORT_DIRECTORY_ENV, REPORT_DIRECTORY))
    if not root.is_absolute():
        root = REPOSITORY / root
    directory = root / f"nextest-{profile}-{uuid.uuid4()}"
    directory.mkdir(parents=True)
    started = start("tests")
    errors: list[str] = []
    original_error: BaseException | None = None
    status: int | None = None
    outcomes: dict[str, Outcome] = {}
    config: Path | None = None
    stdout_path = directory / "stdout.jsonl"
    stderr_path = directory / "stderr.log"
    raw_path = directory / "telemetry.jsonl"
    omitted_log = 0
    served = Collector()
    artifact = ArtifactStore(raw_path, outcomes)
    reader: EventReader | None = None
    try:
        outcomes, config = prepare(command, directory)
        artifact = ArtifactStore(raw_path, outcomes)
        cases = CaseStore(observe=artifact.observe)
        reader = EventReader(outcomes, cases, artifact)
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            counts = [0, 0]

            def retain(data: bytes, index: int) -> None:
                nonlocal omitted_log
                kept = data[: max(0, RUN_LOG_BYTES_MAX - counts[index])]
                (stdout if index == 0 else stderr).write(kept)
                counts[index] += len(kept)
                omitted_log += len(data) - len(kept)
                if index == 0:
                    assert reader is not None
                    reader.feed(data)

            with collector(cases=cases, request_observer=artifact.request) as served:
                command.with_env(
                    **served.environment(source=command.environment()),
                    RIFT_SCOPED_RECORDER_STREAM="1",
                )
                try:
                    status = asyncio.run(
                        command.stream(
                            lambda data: retain(data, 0),
                            stderr=lambda data: retain(data, 1),
                            exit_wait_seconds=EXIT_WAIT_SECONDS,
                        )
                    ).status
                except BaseException as error:  # noqa: BLE001 - retain evidence before re-raising.
                    original_error = error
                    if isinstance(error, CommandFailed):
                        status = error.status
                    else:
                        errors.append(f"incomplete execution: {error}")
            # Collector has completed its bounded shutdown before the final snapshot.
            if reader.pending:
                reader.line(bytes(reader.pending))
            errors.extend(reader.errors)
            if reader.error_count > len(reader.errors):
                errors.append(
                    f"{reader.error_count - len(reader.errors)} further result errors"
                )
            if artifact.request_errors:
                errors.append(f"collection requests failed: {artifact.request_errors}")
            if served.dropped().bodies or served.dropped().kinds:
                errors.append(f"collection refused data: {served.dropped().counts()}")
            if artifact.write_error:
                errors.append(f"artifact write error: {artifact.write_error}")
            if artifact.omitted or artifact.unassigned or omitted_log:
                errors.append(
                    f"collection omitted={artifact.omitted} unassigned={artifact.unassigned} output_bytes_omitted={omitted_log}"
                )
    except BaseException as error:  # noqa: BLE001 - also save setup/cancellation evidence.
        if original_error is None:
            original_error = error
        if isinstance(error, CommandFailed):
            status = error.status
        errors.append(f"{'setup' if config is None else 'collection'} error: {error}")
    finally:
        if config is not None:
            export = (
                CargoCommand(
                    "nextest",
                    "store",
                    "export",
                    "latest",
                    "--user-config-file",
                    config,
                    "--archive-file",
                    directory / "recording.zip",
                )
                .with_environment(command.environment() or os.environ)
                .with_cwd(command.directory)
            )
            try:
                export.output_bytes(echo_stderr=False)
            except Exception as error:  # noqa: BLE001 - preserve original execution status.
                errors.append(f"recording export error: {error}")

    failed = [outcome for outcome in outcomes.values() if outcome.failed]
    launched = sum(outcome.started for outcome in outcomes.values())
    incomplete = sum(
        outcome.started and outcome.result is None for outcome in outcomes.values()
    )
    unlaunched = sum(
        outcome.case.selected and not outcome.started for outcome in outcomes.values()
    )
    if not launched:
        errors.append("no tests launched")
    if incomplete:
        errors.append(f"{incomplete} tests have no terminal result")
    if unlaunched:
        errors.append(f"{unlaunched} selected tests never started")
    if status not in (0, None) and not failed:
        errors.append(f"runner exited {status}; inspect {stderr_path}")
    elapsed = time.monotonic() - started
    summary = RunSummary(
        invocation=directory.name,
        command=str(command),
        platform=platform.platform(),
        profile=profile,
        status=status,
        elapsed_seconds=elapsed,
        launched=launched,
        attempts_launched=launched,
        succeeded=launched - len(failed),
        failed=len(failed) - incomplete,
        incomplete=incomplete,
        unlaunched=unlaunched,
        attempts=tuple(artifact.identities.values()),
        errors=tuple(errors),
        received=artifact.received,
        retained=artifact.retained,
        omitted=artifact.omitted,
        received_bytes=artifact.received_bytes,
        unassigned=artifact.unassigned,
        cache_evictions=served.dropped(),
        request_cache_evictions=served.requests.dropped,
        log_bytes_omitted=omitted_log,
        artifacts=tuple(str(path) for path in directory.iterdir() if path.is_file()),
    )
    (directory / "run.json").write_text(
        summary.model_dump_json(indent=2, by_alias=True) + "\n", encoding="utf-8"
    )
    console_bytes = 0
    console_omitted = 0
    try:
        telemetry = artifact.telemetry(failed) if failed else {}
    except (OSError, ValidationError) as error:
        errors.append(f"artifact read error: {str(error)[:LINE_CHARS_MAX]}")
        telemetry = {outcome.case.full_name: CaseTelemetry() for outcome in failed}
    if errors:
        collection_report(directory, errors, served, artifact)
    for outcome in failed:
        held = telemetry[outcome.case.full_name]
        report = failure_report(outcome, held, artifact, command, profile)
        filename = re.sub(r"[^A-Za-z0-9._-]", "_", outcome.case.full_name)
        path = (
            directory
            / f"{filename[:120]}-{uuid.uuid5(uuid.NAMESPACE_OID, outcome.case.full_name)}.txt"
        )
        path.write_text(report, encoding="utf-8")
        result = outcome.result
        rows = timeline(held)[-CONSOLE_EVIDENCE_MAX:]
        console = "\n".join(
            [
                f"{outcome.case.test} failed!",
                "  Error:",
                (result.stdout or result.reason)
                if result
                else "No terminal result received.",
                "  Logs and metrics:",
                *rows,
                f"  Artifacts: {path}",
            ]
        )
        data = console.encode("utf-8")[: max(0, CONSOLE_BYTES_MAX - console_bytes)]
        console_bytes += len(data)
        console_omitted += len(console.encode("utf-8")) - len(data)
        if data:
            print(data.decode("utf-8", errors="ignore"), flush=True)
    if console_omitted:
        print(
            f"console bytes omitted={console_omitted}; complete bounded reports: {directory}",
            flush=True,
        )
    summary = summary.model_copy(
        update={
            "errors": tuple(errors),
            "artifacts": tuple(
                str(path) for path in directory.iterdir() if path.is_file()
            ),
        }
    )
    (directory / "run.json").write_text(
        summary.model_dump_json(indent=2, by_alias=True) + "\n", encoding="utf-8"
    )
    if errors or failed or status != 0:
        print(
            f"run failed: cases={len(failed)}; errors={len(errors)}; artifacts={directory}",
            flush=True,
        )
        for error in errors[:CONSOLE_EVIDENCE_MAX]:
            print(cut(error), flush=True)
        if original_error is not None:
            raise original_error
        raise CommandFailed(command, status or 1, f"evidence: {directory}")
    print(success_line(launched, elapsed), flush=True)
