"""Collect Rift's exported spans and metric points in memory, summarize them, and select them by time.

`rift` exports its `traced!` spans and its metrics over OTLP/HTTP, protobuf-encoded
(`opentelemetry-otlp`'s `http-proto` feature), once `OTEL_EXPORTER_OTLP_ENDPOINT` names a
receiver. The exporter appends `/v1/traces` and
`/v1/metrics` to that base URL. The collector here is that receiver. It accepts
`POST /v1/traces` and `POST /v1/metrics` and keeps, in memory:

- every span received, with its name, trace and span identifiers, start and end,
  attributes, and the `service.instance.id` of the process that sent it, at most
  `SPANS_MAX`, and its duration for the per-operation summary;
- every metric data point retained, with its instrument's name, kind, and unit, its
  attributes and resource attributes, the `service.instance.id` of the process that sent
  it, its value (a histogram's count, sum, and buckets),
  `start_time_unix_nano`, `time_unix_nano`, and aggregation temporality, at most
  `POINTS_MAX`. Identical cumulative samples with the same resource and timestamps are
  kept once;
- the latest value of each metric series, for a per-test report.

Bounds: a request body, encoded and decompressed, is at most `BODY_BYTES_MAX` bytes; the
summary keeps at most `METRICS_MAX` metric names and `SERIES_MAX` series per name; the
point, span, and duration stores drop their oldest entry past their bound. Whatever a
bound refused or dropped is counted in `Dropped`, so a report cannot describe an
incomplete capture as complete.

`collector()` serves the receiver on `127.0.0.1`, on a port the system picks, from a
thread of the calling process, and stops it on exit. The stores take a lock, so the
caller reads them while the thread writes.
"""

from __future__ import annotations

import json
import math
import os
import signal
import socket
import sys
import threading
import time
import zlib
from collections import deque
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime, timedelta

import uvicorn
from google.protobuf.message import DecodeError
from opentelemetry.proto.collector.logs.v1.logs_service_pb2 import (
    ExportLogsServiceRequest,
    ExportLogsServiceResponse,
)
from opentelemetry.proto.collector.metrics.v1.metrics_service_pb2 import (
    ExportMetricsServiceRequest,
    ExportMetricsServiceResponse,
)
from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import (
    ExportTraceServiceRequest,
    ExportTraceServiceResponse,
)
from opentelemetry.proto.common.v1.common_pb2 import AnyValue, KeyValue
from opentelemetry.proto.logs.v1.logs_pb2 import LogRecord
from opentelemetry.proto.metrics.v1.metrics_pb2 import (
    AGGREGATION_TEMPORALITY_CUMULATIVE,
    AGGREGATION_TEMPORALITY_DELTA,
    HistogramDataPoint,
    Metric,
    NumberDataPoint,
)
from starlette.applications import Starlette
from starlette.requests import ClientDisconnect, Request
from starlette.responses import JSONResponse, PlainTextResponse, Response
from starlette.routing import Route

TRACES_PATH = "/v1/traces"
METRICS_PATH = "/v1/metrics"
LOGS_PATH = "/v1/logs"
CASE_LOGS_PATH = "/test/case/logs"
CASE_METRICS_PATH = "/test/case/metrics"
PROTOBUF = "application/x-protobuf"
BODY_BYTES_MAX = 8 * 1024 * 1024
CASE_LOG_SNAPSHOT_BYTES_MAX = 1024 * 1024
CASE_METRIC_SNAPSHOT_BYTES_MAX = 1024 * 1024
METRICS_MAX = 512
SERIES_MAX = 256
GZIP_WINDOW = 31
# Metric data points kept for time selection. Rift declared 47 instruments on 2026-10-05;
# at a few series each and one export a second, a run sends on the order of 150 points a
# second, so the bound holds the newest few minutes, longer than one failure window. An
# estimate, not a measurement.
POINTS_MAX = 32_768
# Spans kept for time selection and the join with log records by request.
SPANS_MAX = 8_192
# Log records kept for time selection, as many as the metric points.
LOGS_MAX = 32_768
# Span durations kept for the per-operation summary.
DURATIONS_MAX = 262_144
# The interval of the server's metric reader and span batch processor, in milliseconds,
# the unit both variables take. A steady corpus read takes 1.56 seconds at the median, so
# a window of one read holds at least one export.
EXPORT_INTERVAL_MS = 1_000
LOOPBACK = "127.0.0.1"
# What `collector()` waits for the receiver to accept connections, and for its thread to
# end after the stop.
COLLECTOR_START_SECONDS = 5.0
COLLECTOR_STOP_SECONDS = 5.0
START_POLL_SECONDS = 0.005
# uvicorn's bound on open connections draining at stop, in whole seconds.
GRACEFUL_STOP_SECONDS = 2
# The attribute that names a span's MCP request; a printed log record names it `req`.
SPAN_REQUEST_KEY = "request_id"
# The attribute that holds the duration a Rift operation records on completion.
ELAPSED_KEY = "elapsed_ms"
# The resource attribute that names the process a span or point came from.
INSTANCE_KEY = "service.instance.id"
# The resource attribute that carries the sending process's identifier.
PID_KEY = "process.pid"
# The resource attribute a test harness sets through `OTEL_RESOURCE_ATTRIBUTES` on every
# process it spawns, naming the test the process serves.
TEST_CASE_KEY = "test.case.name"
# What `CaseStore` keeps per test: points, spans, and log records, the newest of each.
CASE_POINTS_MAX = 20_000
CASE_SPANS_MAX = 5_000
CASE_LOGS_MAX = 20_000
CASE_LOG_SNAPSHOT_MAX = 256
CASE_METRIC_SNAPSHOT_MAX = 256
# Tests `CaseStore` holds at once; a test past it is counted, not kept.
CASES_MAX = 512
# Export requests kept for a failed test report. Requests arrive before their resource
# attributes can name a test, so the bound applies before decode too.
EXPORT_REQUESTS_MAX = 1_024
EXPORT_REQUEST_IDENTITIES_MAX = 8
EXPORT_REQUEST_REPORT_MAX = 128
REQUEST_IDENTITY_CHARS_MAX = 128
# Attributes `tracing-opentelemetry` 0.34.0 puts on every span, which a span's line
# leaves out, as received from `rift` on 2026-10-05.
SPAN_KEYS_OMITTED = frozenset(["target", "busy_ns", "idle_ns"])
SPAN_KEY_PREFIXES_OMITTED = ("code.", "thread.")

Attributes = tuple[tuple[str, str], ...]
# An instrumentation scope's name and version.
Scope = tuple[str, str]


@dataclass(frozen=True, slots=True)
class OperationTiming:
    """One operation's span count and duration distribution, in milliseconds."""

    operation: str
    count: int
    total_ms: float
    p50_ms: float
    p95_ms: float
    max_ms: float

    def as_json_line(self) -> str:
        """This timing as one compact JSON object, ready to print as a JSON line."""
        return json.dumps(
            {
                "operation": self.operation,
                "count": self.count,
                "total_ms": round(self.total_ms, 3),
                "p50_ms": round(self.p50_ms, 3),
                "p95_ms": round(self.p95_ms, 3),
                "max_ms": round(self.max_ms, 3),
            }
        )


def percentile(values: list[float], fraction: float) -> float:
    """The `fraction`-th percentile of `values` by nearest-rank, ascending."""
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(fraction * len(ordered)))
    return ordered[index]


def summarize(durations: Mapping[str, list[float]]) -> list[OperationTiming]:
    """One [`OperationTiming`] per operation name, highest total duration first."""
    summaries = [
        OperationTiming(
            operation=name,
            count=len(values),
            total_ms=sum(values),
            p50_ms=percentile(values, 0.50),
            p95_ms=percentile(values, 0.95),
            max_ms=max(values),
        )
        for name, values in durations.items()
        if values
    ]
    summaries.sort(key=lambda summary: summary.total_ms, reverse=True)
    return summaries


@dataclass(slots=True)
class Dropped:
    """What the bounds refused or dropped, by reason."""

    bodies: int = 0
    metric_names: int = 0
    series: int = 0
    points: int = 0
    kinds: int = 0
    spans: int = 0
    durations: int = 0
    logs: int = 0

    def counts(self) -> dict[str, int]:
        """Each count by its reason."""
        return {
            "bodies": self.bodies,
            "metric_names": self.metric_names,
            "series": self.series,
            "points": self.points,
            "kinds": self.kinds,
            "spans": self.spans,
            "durations": self.durations,
            "logs": self.logs,
        }

    def any(self) -> bool:
        """Whether a bound refused or dropped anything."""
        return any(self.counts().values())

    def as_json_line(self) -> str:
        """These counts as one compact JSON object under `dropped`."""
        return json.dumps({"dropped": self.counts()})


@dataclass(frozen=True, slots=True)
class RequestIdentity:
    """One decoded resource's test, process, and service instance identifiers."""

    test_case: str | None
    pid: str | None
    instance: str | None
    truncated: bool = False


@dataclass(frozen=True, slots=True)
class StoreReceipt:
    """Decode and store times plus bounded identities from one OTLP request."""

    decoded_unix_nano: int
    stored_unix_nano: int
    identities: tuple[RequestIdentity, ...]
    identities_omitted: int


@dataclass(slots=True)
class ExportRequest:
    """One bounded OTLP/HTTP request, including requests that fail before decode."""

    request_id: int
    path: str
    arrived_unix_nano: int
    body_read_unix_nano: int | None = None
    body_decoded_unix_nano: int | None = None
    decoded_unix_nano: int | None = None
    stored_unix_nano: int | None = None
    handler_finished_unix_nano: int | None = None
    encoded_bytes: int | None = None
    decoded_bytes: int | None = None
    intended_status: int | None = None
    outcome: str = "pending"
    identities: tuple[RequestIdentity, ...] = ()
    identities_omitted: int = 0


@dataclass(frozen=True, slots=True)
class RequestSnapshot:
    """The requests selected for one failure, with every bound count."""

    requests: tuple[ExportRequest, ...]
    received: int
    retained: int
    dropped: int
    omitted: int


class RequestStore:
    """The newest bounded OTLP/HTTP requests, including unassigned requests."""

    def __init__(self, observe: Callable[[ExportRequest], None] | None = None) -> None:
        self.requests: deque[ExportRequest] = deque(maxlen=EXPORT_REQUESTS_MAX)
        self.received = 0
        self.dropped = 0
        self.lock = threading.Lock()
        self.observe = observe

    def begin(self, path: str) -> ExportRequest:
        """Keeps request arrival before content validation or body decode."""
        with self.lock:
            request = ExportRequest(
                request_id=self.received + 1,
                path=path,
                arrived_unix_nano=time.time_ns(),
            )
            self.received += 1
            if len(self.requests) == self.requests.maxlen:
                self.dropped += 1
            self.requests.append(request)
            return request

    def update(self, request: ExportRequest, **fields: object) -> None:
        """Updates one retained request while its route advances."""
        with self.lock:
            for name, value in fields.items():
                setattr(request, name, value)

    def finish(
        self, request: ExportRequest, status: int, outcome: str
    ) -> ExportRequest:
        """Records handler status and finish time before returning a response."""
        self.update(
            request,
            handler_finished_unix_nano=time.time_ns(),
            intended_status=status,
            outcome=outcome,
        )
        if self.observe is not None:
            self.observe(replace(request))
        return request

    def for_failure(self, names: Sequence[str], pids: set[str]) -> RequestSnapshot:
        """Selects decoded requests for the failed test's cases or registered processes.

        Requests without decoded identities stay visible, bounded to the newest quarter
        of the report limit.
        """
        return self._select(names, pids, include_unassigned=True)

    def for_cases(self, names: Sequence[str], pids: set[str]) -> RequestSnapshot:
        """Selects decoded requests attributed to the given test cases."""
        return self._select(names, pids, include_unassigned=False)

    def for_run(self, failed_ids: set[int]) -> RequestSnapshot:
        """Returns attributed requests and failed unassigned requests for one test run."""
        with self.lock:
            held = tuple(
                replace(request)
                for request in self.requests
                if request.identities or request.request_id in failed_ids
            )
            return RequestSnapshot(
                requests=held,
                received=self.received,
                retained=len(held),
                dropped=self.dropped,
                omitted=0,
            )

    def _select(
        self, names: Sequence[str], pids: set[str], *, include_unassigned: bool
    ) -> RequestSnapshot:
        with self.lock:
            held = tuple(replace(request) for request in self.requests)
            received = self.received
            dropped = self.dropped
        cases = set(names)
        unassigned: list[ExportRequest] = []
        matched: list[ExportRequest] = []
        for request in held:
            if any(
                identity.test_case in cases
                or (identity.pid is not None and identity.pid in pids)
                for identity in request.identities
            ):
                matched.append(request)
            elif include_unassigned and not request.identities:
                unassigned.append(request)
        unassigned_limit = EXPORT_REQUEST_REPORT_MAX // 4
        selected = [
            *matched,
            *(unassigned[-unassigned_limit:] if include_unassigned else ()),
        ]
        selected.sort(key=lambda request: request.arrived_unix_nano)
        omitted = max(0, len(selected) - EXPORT_REQUEST_REPORT_MAX)
        requests = tuple(selected[-EXPORT_REQUEST_REPORT_MAX:])
        return RequestSnapshot(
            requests=requests,
            received=received,
            retained=len(held),
            dropped=dropped,
            omitted=omitted,
        )


def value_text(value: AnyValue) -> str:
    """An attribute value as text: a map as `{key=value ...}`, an array as
    `[value ...]`; a value of a kind the text does not read, such as bytes, becomes its
    kind's name."""
    kind = value.WhichOneof("value")
    if kind == "bool_value":
        return str(value.bool_value).lower()
    if kind in ("string_value", "int_value", "double_value"):
        return str(getattr(value, kind))
    if kind == "kvlist_value":
        pairs = " ".join(
            f"{item.key}={value_text(item.value)}" for item in value.kvlist_value.values
        )
        return "{" + pairs + "}"
    if kind == "array_value":
        return (
            "[" + " ".join(value_text(item) for item in value.array_value.values) + "]"
        )
    return str(kind)


def attribute_key(attributes: Iterable[KeyValue]) -> Attributes:
    """Attributes as a sorted, hashable key, each value as `value_text`."""
    return tuple(sorted((item.key, value_text(item.value)) for item in attributes))


def instance_of(resource: Attributes) -> str:
    """The `service.instance.id` resource attribute that names the sending process; empty
    when the resource carries none."""
    return dict(resource).get(INSTANCE_KEY, "")


def test_of(resource: Attributes) -> str:
    """The `test.case.name` resource attribute; empty when the resource carries none."""
    return dict(resource).get(TEST_CASE_KEY, "")


def request_identity(resource: Attributes) -> RequestIdentity | None:
    """A request identity only when decode supplied a test, process, or instance."""
    values = dict(resource)
    test_case = values.get(TEST_CASE_KEY) or None
    pid = values.get(PID_KEY) or None
    instance = values.get(INSTANCE_KEY) or None
    if test_case is None and pid is None and instance is None:
        return None
    fields = tuple(
        None if value is None else value[:REQUEST_IDENTITY_CHARS_MAX]
        for value in (test_case, pid, instance)
    )
    truncated = any(
        value is not None and len(value) > REQUEST_IDENTITY_CHARS_MAX
        for value in (test_case, pid, instance)
    )
    return RequestIdentity(*fields, truncated=truncated)


def store_receipt(identities: Iterable[Attributes], decoded: int) -> StoreReceipt:
    """Bounds decoded request identities and records completion after store work."""
    kept: list[RequestIdentity] = []
    omitted = 0
    for resource in identities:
        identity = request_identity(resource)
        if identity is None or identity in kept:
            continue
        if len(kept) < EXPORT_REQUEST_IDENTITIES_MAX:
            kept.append(identity)
        else:
            omitted += 1
    return StoreReceipt(
        decoded_unix_nano=decoded,
        stored_unix_nano=time.time_ns(),
        identities=tuple(kept),
        identities_omitted=omitted,
    )


def process_text(resource: Attributes) -> str:
    """The `pid=<process.pid>  ` prefix of a printed line; empty without one."""
    pid = dict(resource).get(PID_KEY, "")
    return f"pid={pid}  " if pid else ""


def instance_text(instance: str) -> str:
    """The `service.instance.id=<id>  ` prefix of a printed line; empty without an id."""
    return f"{INSTANCE_KEY}={instance}  " if instance else ""


def scope_text(scope: Scope) -> str:
    """The `otel.scope.name=<name> otel.scope.version=<version>  ` part of a printed
    line; empty for a point that arrived under an unnamed scope."""
    name, version = scope
    if not name:
        return ""
    return f"otel.scope.name={name} otel.scope.version={version}  "


def number_value(point: NumberDataPoint) -> float:
    """A number data point's value, whichever of `as_double` and `as_int` it set."""
    if point.WhichOneof("value") == "as_int":
        return float(point.as_int)
    return float(point.as_double)


def nanoseconds(instant: str) -> int:
    """An ISO 8601 instant with an offset, as `utc_now` prints it, in Unix nanoseconds."""
    moment = datetime.fromisoformat(instant)
    return (moment - EPOCH) // timedelta(microseconds=1) * 1_000


EPOCH = datetime(1970, 1, 1, tzinfo=UTC)


def stamp(unix_nano: int) -> str:
    """Unix nanoseconds in the layout a printed log record starts with."""
    moment = EPOCH + timedelta(microseconds=unix_nano // 1_000)
    return moment.strftime("%Y-%m-%d %H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"


def number_text(value: float) -> str:
    """A value without a trailing `.0` when it is whole."""
    return str(int(value)) if value.is_integer() else f"{value:.6g}"


def fields_text(attributes: Attributes) -> str:
    """Attributes as `key=value` pairs, in key order."""
    return " ".join(f"{key}={value}" for key, value in attributes)


TEMPORALITY = {
    AGGREGATION_TEMPORALITY_CUMULATIVE: "cumulative",
    AGGREGATION_TEMPORALITY_DELTA: "delta",
}


@dataclass(frozen=True, slots=True)
class MetricPoint:
    """One metric data point as received.

    `value` is a sum's or gauge's value, or a histogram's sum; `count`, `bounds`, and
    `bucket_counts` are a histogram's and stay empty otherwise. `temporality` is
    `cumulative` or `delta` for a sum or histogram and empty for a gauge. The printed line
    names the sending process by its `service.instance.id`, so the points of two servers
    stay apart. `scope` is the instrumentation scope the point arrived under, its name and
    version: the crate that emitted it.
    """

    name: str
    kind: str
    unit: str
    temporality: str
    attributes: Attributes
    resource: Attributes
    start_time_unix_nano: int
    time_unix_nano: int
    value: float
    count: int | None = None
    bounds: tuple[float, ...] = ()
    bucket_counts: tuple[int, ...] = ()
    scope: Scope = ("", "")

    @property
    def instance(self) -> str:
        """The `service.instance.id` of the process that sent the point; empty when its
        resource carries none."""
        return instance_of(self.resource)

    def line(self) -> str:
        """The point as one line in the layout of a printed log record: time, kind,
        name, the sending process, the instrumentation scope as `otel.scope.name` and
        `otel.scope.version`, attributes, then the value.

        A histogram prints its nonempty buckets as `<=bound:count`, the last one
        `>bound:count`.
        """
        if self.count is None:
            reading = f"value={number_text(self.value)}"
        else:
            buckets = ",".join(
                (
                    f"<={number_text(self.bounds[index])}:{count}"
                    if index < len(self.bounds)
                    else f">{number_text(self.bounds[-1]) if self.bounds else '-inf'}:{count}"
                )
                for index, count in enumerate(self.bucket_counts)
                if count
            )
            reading = f"count={self.count} sum={number_text(self.value)}" + (
                f" buckets={buckets}" if buckets else ""
            )
        extra = " ".join(
            part
            for part in (f"unit={self.unit}" if self.unit else "", self.temporality)
            if part
        )
        context = fields_text(self.attributes)
        return (
            f"{stamp(self.time_unix_nano)} {self.kind:<9} {self.name}   "
            + instance_text(self.instance)
            + scope_text(self.scope)
            + (f"{context}  " if context else "")
            + reading
            + (f" {extra}" if extra else "")
        )


@dataclass(frozen=True, slots=True)
class SpanRecord:
    """One span as received: its name, identifiers in hex, start and end, attributes, and
    the `service.instance.id` of the process that sent it, empty when its resource carries
    none."""

    name: str
    trace_id: str
    span_id: str
    start_time_unix_nano: int
    end_time_unix_nano: int
    attributes: Attributes
    instance: str = ""
    resource: Attributes = ()
    parent_span_id: str = ""
    status_code: int = 0
    status_message: str = ""
    kind: int = 0

    @property
    def duration_ms(self) -> float:
        """The span's duration in milliseconds.

        A non-negative number in the span's `elapsed_ms` attribute, the duration a
        Rift operation records on completion, wins; otherwise the end and start
        timestamps give it.
        """
        recorded = dict(self.attributes).get(ELAPSED_KEY)
        if recorded is not None:
            try:
                elapsed = float(recorded)
            except ValueError:
                elapsed = -1.0
            if math.isfinite(elapsed) and elapsed >= 0.0:
                return elapsed
        return (self.end_time_unix_nano - self.start_time_unix_nano) / 1_000_000

    @property
    def request_id(self) -> str | None:
        """The span's `request_id` attribute; None when it carries none."""
        return dict(self.attributes).get(SPAN_REQUEST_KEY)

    def line(self, records: int | None = None) -> str:
        """The span as one line in the layout of a printed log record, at its end.

        `records`, when given, is the count of log records of the same request. The
        attributes `tracing-opentelemetry` adds to every span, its source location,
        thread, target, and busy and idle time, are left out.
        """
        context = fields_text(
            tuple(
                (key, value)
                for key, value in self.attributes
                if key not in SPAN_KEYS_OMITTED
                and not key.startswith(SPAN_KEY_PREFIXES_OMITTED)
            )
        )
        return (
            f"{stamp(self.end_time_unix_nano)} span      {self.name}   "
            + instance_text(self.instance)
            + (f"{context}  " if context else "")
            + f"elapsed={self.duration_ms:.3f}ms"
            + (f" records={records}" if records is not None else "")
        )


def within(time_unix_nano: int, since: int | None, until: int | None) -> bool:
    """Whether a time falls in `[since, until)`; a missing bound is open."""
    return (since is None or time_unix_nano >= since) and (
        until is None or time_unix_nano < until
    )


class SpanStore:
    """Every span received: the newest `SPANS_MAX` whole, the newest `DURATIONS_MAX`
    durations by operation."""

    def __init__(
        self, spans_max: int = SPANS_MAX, tests: CaseStore | None = None
    ) -> None:
        self.durations: dict[str, deque[float]] = {}
        self.spans: deque[SpanRecord] = deque(maxlen=spans_max)
        self.dropped = Dropped()
        self.received = 0
        self.kept_durations = 0
        self.lock = threading.Lock()
        self.tests = tests

    def record(self, body: bytes) -> StoreReceipt:
        """Decodes one export request, keeps its spans, and reports its resources."""
        request = ExportTraceServiceRequest.FromString(body)
        decoded = time.time_ns()
        with self.lock:
            for resource_spans in request.resource_spans:
                resource = attribute_key(resource_spans.resource.attributes)
                instance = instance_of(resource)
                for scope_spans in resource_spans.scope_spans:
                    for span in scope_spans.spans:
                        self.keep(
                            SpanRecord(
                                name=span.name,
                                trace_id=span.trace_id.hex(),
                                span_id=span.span_id.hex(),
                                start_time_unix_nano=span.start_time_unix_nano,
                                end_time_unix_nano=span.end_time_unix_nano,
                                attributes=attribute_key(span.attributes),
                                instance=instance,
                                resource=resource,
                                parent_span_id=span.parent_span_id.hex(),
                                status_code=span.status.code,
                                status_message=span.status.message,
                                kind=span.kind,
                            )
                        )
        return store_receipt(
            (
                attribute_key(resource_spans.resource.attributes)
                for resource_spans in request.resource_spans
            ),
            decoded,
        )

    def keep(self, span: SpanRecord) -> None:
        """Keeps one span, dropping the oldest past a bound. The caller holds the lock."""
        self.received += 1
        if self.tests is not None:
            self.tests.keep(span.resource, span)
        if len(self.spans) == self.spans.maxlen:
            self.dropped.spans += 1
        self.spans.append(span)
        if self.kept_durations >= DURATIONS_MAX:
            self.dropped.durations += 1
            return
        self.kept_durations += 1
        self.durations.setdefault(span.name, deque()).append(span.duration_ms)

    def summary(self) -> list[OperationTiming]:
        """The per-operation summary of every span duration kept."""
        with self.lock:
            durations = {name: list(values) for name, values in self.durations.items()}
        return summarize(durations)

    def between(self, since: int | None, until: int | None) -> list[SpanRecord]:
        """The kept spans that end in `[since, until)`, in Unix nanoseconds, oldest first."""
        with self.lock:
            found = [
                span
                for span in self.spans
                if within(span.end_time_unix_nano, since, until)
            ]
        return sorted(found, key=lambda span: span.end_time_unix_nano)


@dataclass(frozen=True, slots=True)
class MetricSummary:
    """One metric name: its kind, unit, series, and the sum of its series' latest values.

    A histogram also carries the count of its recorded values; its `value` is their
    sum.
    """

    metric: str
    kind: str
    unit: str
    series: int
    points: int
    value: float
    count: int | None

    def as_json_line(self) -> str:
        """This summary as one compact JSON object, ready to print as a JSON line."""
        line: dict[str, object] = {
            "metric": self.metric,
            "kind": self.kind,
            "unit": self.unit,
            "series": self.series,
            "points": self.points,
            "value": round(self.value, 3),
        }
        if self.count is not None:
            line["count"] = self.count
        return json.dumps(line)


@dataclass(slots=True)
class MetricSeries:
    """One metric name's series, each as its latest or accumulated value and count, keyed
    by resource, start time, instrumentation scope, and attributes."""

    kind: str
    unit: str
    values: dict[tuple[Attributes, int, Scope, Attributes], tuple[float, int]] = field(
        default_factory=dict
    )
    points: int = 0


class MetricStore:
    """Metric data points retained: the newest `POINTS_MAX` whole, and the latest value
    of every bounded series by metric name.

    Resource, start time, scope, and attributes identify a series. Identical cumulative
    samples with the same series and timestamp are kept once. A gauge and a cumulative
    sum or histogram replace a series' value with the newest data point; a delta sum or
    histogram adds each data point to it. The Rust exporter's default temporality is
    cumulative.
    """

    def __init__(
        self, points_max: int = POINTS_MAX, tests: CaseStore | None = None
    ) -> None:
        self.metrics: dict[str, MetricSeries] = {}
        self.points: deque[MetricPoint] = deque(maxlen=points_max)
        self.cumulative_points: set[MetricPoint] = set()
        self.dropped = Dropped()
        self.received = 0
        self.lock = threading.Lock()
        self.tests = tests

    def record(self, body: bytes) -> StoreReceipt:
        """Decodes one export request, keeps its points, and reports its resources."""
        request = ExportMetricsServiceRequest.FromString(body)
        decoded = time.time_ns()
        with self.lock:
            for resource_metrics in request.resource_metrics:
                resource = attribute_key(resource_metrics.resource.attributes)
                for scope_metrics in resource_metrics.scope_metrics:
                    scope = (scope_metrics.scope.name, scope_metrics.scope.version)
                    for metric in scope_metrics.metrics:
                        self.keep(metric, resource, scope)
        return store_receipt(
            (
                attribute_key(resource_metrics.resource.attributes)
                for resource_metrics in request.resource_metrics
            ),
            decoded,
        )

    def keep(
        self, metric: Metric, resource: Attributes = (), scope: Scope = ("", "")
    ) -> None:
        """Keeps the data points of one metric of a supported kind, under the
        instrumentation `scope` it arrived in; the caller holds the lock. A summary or
        exponential histogram is counted under `kinds`."""
        which = metric.WhichOneof("data")
        temporality = ""
        if which == "gauge":
            points = metric.gauge.data_points
        elif which == "sum":
            points = metric.sum.data_points
            temporality = TEMPORALITY.get(metric.sum.aggregation_temporality, "")
        elif which == "histogram":
            points = metric.histogram.data_points
            temporality = TEMPORALITY.get(metric.histogram.aggregation_temporality, "")
        else:
            self.dropped.kinds += 1
            return
        name = metric.name
        held = self.metrics.get(name)
        if held is None and len(self.metrics) < METRICS_MAX:
            held = MetricSeries(which, metric.unit)
            self.metrics[name] = held
        elif held is None:
            self.dropped.metric_names += 1
        for point in points:
            attributes = attribute_key(point.attributes)
            if isinstance(point, HistogramDataPoint):
                value, count = float(point.sum), int(point.count)
                kept = MetricPoint(
                    name,
                    which,
                    metric.unit,
                    temporality,
                    attributes,
                    resource,
                    point.start_time_unix_nano,
                    point.time_unix_nano,
                    value,
                    count,
                    tuple(point.explicit_bounds),
                    tuple(point.bucket_counts),
                    scope,
                )
            else:
                value, count = number_value(point), 0
                kept = MetricPoint(
                    name,
                    which,
                    metric.unit,
                    temporality,
                    attributes,
                    resource,
                    point.start_time_unix_nano,
                    point.time_unix_nano,
                    value,
                    scope=scope,
                )
            self.received += 1
            if self.tests is not None:
                self.tests.keep(resource, kept)
            duplicate = (
                kept.temporality == "cumulative" and kept in self.cumulative_points
            )
            if duplicate:
                continue
            if self.points.maxlen and len(self.points) == self.points.maxlen:
                expired = self.points.popleft()
                if expired.temporality == "cumulative":
                    self.cumulative_points.discard(expired)
                self.dropped.points += 1
            if self.points.maxlen:
                self.points.append(kept)
                if kept.temporality == "cumulative":
                    self.cumulative_points.add(kept)
            else:
                self.dropped.points += 1
            if held is None:
                continue
            key = (resource, kept.start_time_unix_nano, scope, attributes)
            if key not in held.values and len(held.values) >= SERIES_MAX:
                self.dropped.series += 1
                continue
            held.points += 1
            previous = held.values.get(key, (0.0, 0))
            held.values[key] = (
                (previous[0] + value, previous[1] + count)
                if temporality == "delta"
                else (value, count)
            )

    def between(
        self, since: int | None, until: int | None, name: str | None = None
    ) -> list[MetricPoint]:
        """The kept points with `time_unix_nano` in `[since, until)`, oldest first; only
        those of instrument `name` when given."""
        with self.lock:
            found = [
                point
                for point in self.points
                if (name is None or point.name == name)
                and within(point.time_unix_nano, since, until)
            ]
        return sorted(found, key=lambda point: point.time_unix_nano)

    def summary(self) -> list[MetricSummary]:
        """One [`MetricSummary`] per metric name, ordered by name."""
        with self.lock:
            return [
                MetricSummary(
                    metric=name,
                    kind=held.kind,
                    unit=held.unit,
                    series=len(held.values),
                    points=held.points,
                    value=sum(value for value, _ in held.values.values()),
                    count=(
                        sum(count for _, count in held.values.values())
                        if held.kind == "histogram"
                        else None
                    ),
                )
                for name, held in sorted(self.metrics.items())
            ]


SEVERITY_TEXT = {
    1: "TRACE",
    5: "DEBUG",
    9: "INFO",
    13: "WARN",
    17: "ERROR",
    21: "FATAL",
}


@dataclass(frozen=True, slots=True)
class LogEntry:
    """One OTLP log record as received: its time, severity, body, attributes, the
    resource of the process that sent it, and the trace and span it was recorded in,
    in hex, empty when it was recorded outside a span."""

    time_unix_nano: int
    severity: str
    body: str
    attributes: Attributes
    resource: Attributes
    trace_id: str = ""
    span_id: str = ""

    @property
    def instance(self) -> str:
        """The `service.instance.id` of the process that sent the record."""
        return instance_of(self.resource)

    def line(self) -> str:
        """The record as one line in the layout of a printed log record."""
        context = fields_text(self.attributes)
        span = f"  span={self.span_id}" if self.span_id else ""
        return (
            f"{stamp(self.time_unix_nano)} {self.severity:<9} {self.body}   "
            + process_text(self.resource)
            + instance_text(self.instance)
            + context
            + span
        )


def log_entry(record: LogRecord, resource: Attributes) -> LogEntry:
    """A received `LogRecord` as a [`LogEntry`]: its time, or its observed time when the
    sender set none, and its severity text, or the name of its severity number."""
    severity = record.severity_text or SEVERITY_TEXT.get(
        record.severity_number - (record.severity_number - 1) % 4,
        str(record.severity_number),
    )
    return LogEntry(
        time_unix_nano=record.time_unix_nano or record.observed_time_unix_nano,
        severity=severity,
        body=value_text(record.body),
        attributes=attribute_key(record.attributes),
        resource=resource,
        trace_id=record.trace_id.hex(),
        span_id=record.span_id.hex(),
    )


class LogStore:
    """Every OTLP log record received: the newest `LOGS_MAX`."""

    def __init__(
        self, logs_max: int = LOGS_MAX, tests: CaseStore | None = None
    ) -> None:
        self.logs: deque[LogEntry] = deque(maxlen=logs_max)
        self.dropped = Dropped()
        self.received = 0
        self.lock = threading.Lock()
        self.tests = tests

    def record(self, body: bytes) -> StoreReceipt:
        """Decodes one export request, keeps its logs, and reports its resources."""
        request = ExportLogsServiceRequest.FromString(body)
        decoded = time.time_ns()
        with self.lock:
            for resource_logs in request.resource_logs:
                resource = attribute_key(resource_logs.resource.attributes)
                for scope_logs in resource_logs.scope_logs:
                    for record in scope_logs.log_records:
                        self.keep(log_entry(record, resource))
        return store_receipt(
            (
                attribute_key(resource_logs.resource.attributes)
                for resource_logs in request.resource_logs
            ),
            decoded,
        )

    def keep(self, entry: LogEntry) -> None:
        """Keeps one record, dropping the oldest past the bound. The caller holds the lock."""
        self.received += 1
        if self.tests is not None:
            self.tests.keep(entry.resource, entry)
        if len(self.logs) == self.logs.maxlen:
            self.dropped.logs += 1
        self.logs.append(entry)

    def between(self, since: int | None, until: int | None) -> list[LogEntry]:
        """The kept records with a time in `[since, until)`, oldest first."""
        with self.lock:
            found = [
                entry
                for entry in self.logs
                if within(entry.time_unix_nano, since, until)
            ]
        return sorted(found, key=lambda entry: entry.time_unix_nano)


@dataclass(slots=True)
class CaseTelemetry:
    """What the processes of one test sent: the newest points, spans, and log records
    within their bounds, and what each bound dropped. Identical cumulative points are
    kept once."""

    points: deque[MetricPoint] = field(
        default_factory=lambda: deque(maxlen=CASE_POINTS_MAX)
    )
    cumulative_points: set[MetricPoint] = field(default_factory=set)
    spans: deque[SpanRecord] = field(
        default_factory=lambda: deque(maxlen=CASE_SPANS_MAX)
    )
    logs: deque[LogEntry] = field(default_factory=lambda: deque(maxlen=CASE_LOGS_MAX))
    dropped: Dropped = field(default_factory=Dropped)


class CaseStore:
    """Points, spans, and log records by the `test.case.name` of their sender's resource.

    A runner that knows when each test ends takes a failed test's telemetry with `take`
    and drops a passed test's with `forget`, so the store holds the tests still running.
    A sender that carries no `test.case.name` is counted under `unattributed`; a test past
    `CASES_MAX` held at once is counted under `refused`.
    """

    def __init__(
        self,
        observe: Callable[[MetricPoint | SpanRecord | LogEntry], bool] | None = None,
    ) -> None:
        self.tests: dict[str, CaseTelemetry] = {}
        self.unattributed = 0
        self.refused = 0
        self.lock = threading.Lock()
        self.observe = observe

    def keep(
        self, resource: Attributes, item: MetricPoint | SpanRecord | LogEntry
    ) -> None:
        """Files `item` under the test its sender names."""
        test = test_of(resource)
        with self.lock:
            if self.observe is not None and not self.observe(item):
                return
            if not test:
                self.unattributed += 1
                return
            held = self.tests.get(test)
            if held is None:
                if len(self.tests) >= CASES_MAX:
                    self.refused += 1
                    return
                held = CaseTelemetry()
                self.tests[test] = held
            if isinstance(item, MetricPoint):
                if item.temporality == "cumulative" and item in held.cumulative_points:
                    return
                if len(held.points) == held.points.maxlen:
                    expired = held.points.popleft()
                    if expired.temporality == "cumulative":
                        held.cumulative_points.discard(expired)
                    held.dropped.points += 1
                held.points.append(item)
                if item.temporality == "cumulative":
                    held.cumulative_points.add(item)
            elif isinstance(item, SpanRecord):
                if len(held.spans) == held.spans.maxlen:
                    held.dropped.spans += 1
                held.spans.append(item)
            else:
                if len(held.logs) == held.logs.maxlen:
                    held.dropped.logs += 1
                held.logs.append(item)

    def matching(self, names: Callable[[str], bool]) -> list[str]:
        """The held test names `names` accepts."""
        with self.lock:
            return [test for test in self.tests if names(test)]

    def take(self, test: str) -> CaseTelemetry | None:
        """Removes and answers what `test`'s processes sent; None when none sent anything."""
        with self.lock:
            return self.tests.pop(test, None)

    def log_snapshot(
        self, test: str, limit: int
    ) -> tuple[tuple[LogEntry, ...], int, int] | None:
        """The newest `limit` log records for `test`, dropped count, and retained count."""
        with self.lock:
            held = self.tests.get(test)
            if held is None:
                return None
            logs = tuple(reversed(held.logs))
            return logs[:limit], held.dropped.logs, len(logs)

    def metric_snapshot(
        self, test: str, limit: int
    ) -> tuple[tuple[MetricPoint, ...], int, int] | None:
        """The newest `limit` metric points for `test`, dropped count, and retained count."""
        with self.lock:
            held = self.tests.get(test)
            if held is None:
                return None
            points = tuple(reversed(held.points))
            return points[:limit], held.dropped.points, len(points)

    def forget(self, names: Callable[[str], bool]) -> int:
        """Drops every held test `names` accepts; answers how many."""
        with self.lock:
            gone = [test for test in self.tests if names(test)]
            for test in gone:
                del self.tests[test]
            return len(gone)


def inflate(body: bytes, encoding: str | None) -> bytes | None:
    """The request body, gunzipped when `encoding` says so; None past `BODY_BYTES_MAX`.

    Raises `zlib.error` for a body that is not gzip.
    """
    if len(body) > BODY_BYTES_MAX:
        return None
    if encoding != "gzip":
        return body
    inflater = zlib.decompressobj(GZIP_WINDOW)
    inflated = inflater.decompress(body, BODY_BYTES_MAX + 1)
    if len(inflated) > BODY_BYTES_MAX or inflater.unconsumed_tail:
        return None
    return inflated


async def bounded_body(request: Request) -> tuple[bytes | None, int]:
    """The encoded body or None past its bound, and bytes read before refusal.

    A declared `content-length` past the bound refuses the body before any of it is
    read.
    """
    declared = request.headers.get("content-length", "")
    if declared.isdigit() and int(declared) > BODY_BYTES_MAX:
        return None, 0
    chunks: list[bytes] = []
    size = 0
    async for chunk in request.stream():
        size += len(chunk)
        if size > BODY_BYTES_MAX:
            return None, size
        chunks.append(chunk)
    return b"".join(chunks), size


def receiver(
    spans: SpanStore,
    metrics: MetricStore | None = None,
    logs: LogStore | None = None,
    cases: CaseStore | None = None,
    requests: RequestStore | None = None,
) -> Starlette:
    """The application that feeds OTLP/HTTP export requests into `spans`, `metrics`, and
    `logs`.

    Starlette answers any other path with 404 and any other method with 405. When `cases`
    is supplied, it also serves bounded read-only snapshots of one live test's logs and
    metric points.
    """
    held = metrics if metrics is not None else MetricStore()
    records = logs if logs is not None else LogStore()
    request_records = requests if requests is not None else RequestStore()

    def route(
        path: str,
        keep: Callable[[bytes], StoreReceipt],
        response: bytes,
        noun: str,
    ) -> Route:
        async def export(request: Request) -> Response:
            observed = request_records.begin(path)
            if request.headers.get("content-type", "").split(";")[0] != PROTOBUF:
                request_records.finish(observed, 415, "content-type")
                return PlainTextResponse(f"the collector reads {PROTOBUF}", 415)
            try:
                encoded, encoded_bytes = await bounded_body(request)
                request_records.update(
                    observed,
                    body_read_unix_nano=time.time_ns(),
                    encoded_bytes=encoded_bytes,
                )
                body = (
                    None
                    if encoded is None
                    else inflate(encoded, request.headers.get("content-encoding"))
                )
            except zlib.error:
                request_records.finish(observed, 400, "gzip")
                return PlainTextResponse("the body is not gzip", 400)
            except ClientDisconnect:
                request_records.finish(observed, 400, "disconnect")
                return PlainTextResponse("the exporter disconnected", 400)
            except Exception:
                request_records.finish(observed, 500, "receiver-error")
                raise
            request_records.update(
                observed,
                body_decoded_unix_nano=time.time_ns(),
                decoded_bytes=None if body is None else len(body),
            )
            if body is None:
                with held.lock:
                    held.dropped.bodies += 1
                request_records.finish(observed, 413, "body-bound")
                return PlainTextResponse(
                    f"a body is at most {BODY_BYTES_MAX} bytes, encoded and decoded",
                    413,
                )
            try:
                receipt = keep(body)
            except DecodeError:
                request_records.finish(observed, 400, "protobuf")
                return PlainTextResponse(f"the body is not an {noun}", 400)
            except Exception:
                request_records.finish(observed, 500, "receiver-error")
                raise
            request_records.update(
                observed,
                decoded_unix_nano=receipt.decoded_unix_nano,
                stored_unix_nano=receipt.stored_unix_nano,
                identities=receipt.identities,
                identities_omitted=receipt.identities_omitted,
            )
            request_records.finish(observed, 200, "ok")
            return Response(response, media_type=PROTOBUF)

        return Route(path, export, methods=["POST"])

    routes = [
        route(
            TRACES_PATH,
            spans.record,
            ExportTraceServiceResponse().SerializeToString(),
            "ExportTraceServiceRequest",
        ),
        route(
            METRICS_PATH,
            held.record,
            ExportMetricsServiceResponse().SerializeToString(),
            "ExportMetricsServiceRequest",
        ),
        route(
            LOGS_PATH,
            records.record,
            ExportLogsServiceResponse().SerializeToString(),
            "ExportLogsServiceRequest",
        ),
    ]
    if cases is not None:

        async def case_logs(request: Request) -> Response:
            test = request.query_params.get(TEST_CASE_KEY)
            if not test:
                return PlainTextResponse(
                    f"query parameter {TEST_CASE_KEY} is required", 400
                )
            raw_limit = request.query_params.get("limit", str(CASE_LOG_SNAPSHOT_MAX))
            try:
                limit = int(raw_limit)
            except ValueError:
                return PlainTextResponse("limit must be an integer", 400)
            if not 1 <= limit <= CASE_LOG_SNAPSHOT_MAX:
                return PlainTextResponse(
                    f"limit must be between 1 and {CASE_LOG_SNAPSHOT_MAX}", 400
                )
            snapshot = cases.log_snapshot(test, limit)
            if snapshot is None:
                return PlainTextResponse("test case has no retained telemetry", 404)
            newest_logs, dropped, retained = snapshot
            rows: list[dict[str, object]] = []
            size = 0
            for entry in newest_logs:
                row: dict[str, object] = {
                    "time_unix_nano": entry.time_unix_nano,
                    "severity": entry.severity,
                    "body": entry.body,
                    "attributes": dict(entry.attributes),
                    "resource": dict(entry.resource),
                    "trace_id": entry.trace_id,
                    "span_id": entry.span_id,
                }
                row_size = len(
                    json.dumps(
                        row,
                        ensure_ascii=False,
                        allow_nan=False,
                        separators=(",", ":"),
                    ).encode("utf-8")
                )
                if row_size > CASE_LOG_SNAPSHOT_BYTES_MAX:
                    return PlainTextResponse(
                        "a log record exceeds the case snapshot byte bound", 413
                    )
                if size + row_size > CASE_LOG_SNAPSHOT_BYTES_MAX:
                    break
                rows.append(row)
                size += row_size
            omitted = retained - len(rows)
            return JSONResponse(
                {
                    "test_case": test,
                    "retained": retained,
                    "dropped": dropped,
                    "omitted": omitted,
                    "logs": list(reversed(rows)),
                }
            )

        routes.append(Route(CASE_LOGS_PATH, case_logs, methods=["GET"]))

        async def case_metrics(request: Request) -> Response:
            test = request.query_params.get(TEST_CASE_KEY)
            if not test:
                return PlainTextResponse(
                    f"query parameter {TEST_CASE_KEY} is required", 400
                )
            raw_limit = request.query_params.get("limit", str(CASE_METRIC_SNAPSHOT_MAX))
            try:
                limit = int(raw_limit)
            except ValueError:
                return PlainTextResponse("limit must be an integer", 400)
            if not 1 <= limit <= CASE_METRIC_SNAPSHOT_MAX:
                return PlainTextResponse(
                    f"limit must be between 1 and {CASE_METRIC_SNAPSHOT_MAX}", 400
                )
            snapshot = cases.metric_snapshot(test, limit)
            if snapshot is None:
                return PlainTextResponse("test case has no retained telemetry", 404)
            newest_points, dropped, retained = snapshot
            rows: list[dict[str, object]] = []
            size = 0
            for point in newest_points:
                row: dict[str, object] = {
                    "name": point.name,
                    "kind": point.kind,
                    "unit": point.unit,
                    "temporality": point.temporality,
                    "attributes": dict(point.attributes),
                    "resource": dict(point.resource),
                    "start_time_unix_nano": point.start_time_unix_nano,
                    "time_unix_nano": point.time_unix_nano,
                    "value": point.value,
                    "count": point.count,
                    "bounds": point.bounds,
                    "bucket_counts": point.bucket_counts,
                    "scope": {"name": point.scope[0], "version": point.scope[1]},
                }
                row_size = len(
                    json.dumps(
                        row,
                        ensure_ascii=False,
                        allow_nan=False,
                        separators=(",", ":"),
                    ).encode("utf-8")
                )
                if row_size > CASE_METRIC_SNAPSHOT_BYTES_MAX:
                    return PlainTextResponse(
                        "a metric point exceeds the case snapshot byte bound", 413
                    )
                if size + row_size > CASE_METRIC_SNAPSHOT_BYTES_MAX:
                    break
                rows.append(row)
                size += row_size
            omitted = retained - len(rows)
            return JSONResponse(
                {
                    "test_case": test,
                    "retained": retained,
                    "dropped": dropped,
                    "omitted": omitted,
                    "points": list(reversed(rows)),
                }
            )

        routes.append(Route(CASE_METRICS_PATH, case_metrics, methods=["GET"]))

    return Starlette(routes=routes)


@dataclass(slots=True)
class Collector:
    """The stores one collector fills and the base URL it receives at.

    `endpoint` is empty for stores no receiver serves.
    """

    spans: SpanStore = field(default_factory=SpanStore)
    metrics: MetricStore = field(default_factory=MetricStore)
    endpoint: str = ""
    logs: LogStore = field(default_factory=LogStore)
    cases: CaseStore | None = None
    requests: RequestStore = field(default_factory=RequestStore)

    def environment(
        self,
        test_case: str | None = None,
        source: Mapping[str, str] | None = None,
    ) -> dict[str, str]:
        """The variables that point a server under test at this collector.

        `OTEL_EXPORTER_OTLP_ENDPOINT` is the base URL: Rift installs export only when it
        is set (`crates/rift-tracing/src/otlp.rs`), and the exporter appends
        `/v1/metrics` and `/v1/traces`. `OTEL_METRIC_EXPORT_INTERVAL` is read by the
        async periodic reader and `OTEL_BSP_SCHEDULE_DELAY` by the batch span
        processor's default configuration, both in milliseconds
        (`opentelemetry_sdk` 0.33.0); `OTEL_BLRP_SCHEDULE_DELAY` is the batch log
        processor's. Caller values for those standard interval variables are retained.
        With `test_case`, `OTEL_RESOURCE_ATTRIBUTES` sets `test.case.name`,
        which the SDK's environment resource detector reads into every process's
        resource, so `CaseStore` files what each process sends under its test.
        """
        if not self.endpoint:
            return {}
        inherited = os.environ if source is None else source
        environment = {
            "OTEL_SDK_DISABLED": "false",
            "OTEL_EXPORTER_OTLP_ENDPOINT": self.endpoint,
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": self.endpoint + TRACES_PATH,
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": self.endpoint + LOGS_PATH,
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT": self.endpoint + METRICS_PATH,
            "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL": "http/protobuf",
            "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL": "http/protobuf",
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL": "http/protobuf",
            "OTEL_METRIC_EXPORT_INTERVAL": inherited.get(
                "OTEL_METRIC_EXPORT_INTERVAL", str(EXPORT_INTERVAL_MS)
            ),
            "OTEL_BSP_SCHEDULE_DELAY": inherited.get(
                "OTEL_BSP_SCHEDULE_DELAY", str(EXPORT_INTERVAL_MS)
            ),
            "OTEL_BLRP_SCHEDULE_DELAY": inherited.get(
                "OTEL_BLRP_SCHEDULE_DELAY", str(EXPORT_INTERVAL_MS)
            ),
        }
        if test_case:
            environment["OTEL_RESOURCE_ATTRIBUTES"] = resource_attribute(
                TEST_CASE_KEY, test_case
            )
        return environment

    def dropped(self) -> Dropped:
        """What the bounds of both stores refused or dropped."""
        with self.metrics.lock, self.spans.lock, self.logs.lock:
            metrics, spans, logs = (
                self.metrics.dropped,
                self.spans.dropped,
                self.logs.dropped,
            )
            return Dropped(
                bodies=metrics.bodies + spans.bodies + logs.bodies,
                metric_names=metrics.metric_names,
                series=metrics.series,
                points=metrics.points,
                kinds=metrics.kinds,
                spans=spans.spans,
                durations=spans.durations,
                logs=logs.logs,
            )

    def points(
        self, name: str, since: str | None = None, until: str | None = None
    ) -> list[MetricPoint]:
        """The points of instrument `name` with `time_unix_nano` from `since` until
        `until`, ISO 8601 instants as `utc_now` prints them; oldest first."""
        return self.metrics.between(
            None if since is None else nanoseconds(since),
            None if until is None else nanoseconds(until),
            name,
        )


def resource_attribute(key: str, value: str) -> str:
    """`key=value` as `OTEL_RESOURCE_ATTRIBUTES` carries it.

    `opentelemetry_sdk` 0.33 splits the variable at `,`, each entry at its first `=`,
    trims both sides, and decodes nothing (`src/resource/env.rs`), the parse
    `crates/rift/tests/test_case.rs` encodes for: `,`, `%`, whitespace, and control
    characters become `%XX` of their UTF-8 bytes, every other character stays.
    """
    encoded = "".join(
        "".join(f"%{byte:02X}" for byte in character.encode("utf-8"))
        if character in ",%" or character.isspace() or not character.isprintable()
        else character
        for character in value
    )
    return f"{key}={encoded}"


@contextmanager
def collector(
    points_max: int = POINTS_MAX,
    spans_max: int = SPANS_MAX,
    logs_max: int = LOGS_MAX,
    cases: CaseStore | None = None,
    request_observer: Callable[[ExportRequest], None] | None = None,
) -> Iterator[Collector]:
    """Serves a receiver on `127.0.0.1` from a thread until the block exits.

    The socket is bound before the thread starts, on a port the system picks, and the
    block starts once uvicorn accepts connections, within `COLLECTOR_START_SECONDS`.
    The exit asks uvicorn to stop and joins the thread within
    `COLLECTOR_STOP_SECONDS`; a thread still running then raises `RuntimeError`.
    """
    stores = Collector(
        SpanStore(spans_max, cases),
        MetricStore(points_max, cases),
        logs=LogStore(logs_max, cases),
        cases=cases,
        requests=RequestStore(observe=request_observer),
    )
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        listener.bind((LOOPBACK, 0))
        port = listener.getsockname()[1]
        server = uvicorn.Server(
            uvicorn.Config(
                receiver(
                    stores.spans,
                    stores.metrics,
                    stores.logs,
                    stores.cases,
                    stores.requests,
                ),
                lifespan="off",
                log_config=None,
                log_level="warning",
                access_log=False,
                timeout_graceful_shutdown=GRACEFUL_STOP_SECONDS,
            )
        )
        failures: list[BaseException] = []

        def serve() -> None:
            try:
                server.run(sockets=[listener])
            except BaseException as failure:  # noqa: BLE001 - the start wait reports it.
                failures.append(failure)

        thread = threading.Thread(target=serve, name="rift-otlp-collector", daemon=True)
        thread.start()
        try:
            deadline = time.monotonic() + COLLECTOR_START_SECONDS
            while not server.started:
                if not thread.is_alive() or time.monotonic() > deadline:
                    raise RuntimeError(
                        "the OTLP collector did not start within "
                        f"{COLLECTOR_START_SECONDS}s: {failures or 'no error'}"
                    )
                time.sleep(START_POLL_SECONDS)
            stores.endpoint = f"http://{LOOPBACK}:{port}"
            yield stores
        finally:
            server.should_exit = True
            thread.join(COLLECTOR_STOP_SECONDS)
        if thread.is_alive():
            raise RuntimeError(
                f"the OTLP collector thread outlived its {COLLECTOR_STOP_SECONDS}s stop"
            )
    finally:
        listener.close()


def interrupt(*_: object) -> None:
    """Ends the collector on SIGTERM the way Ctrl-C does, so the summary prints."""
    raise KeyboardInterrupt


def collect(host: str, port: int) -> None:
    """Serves until interrupted, then prints one JSON line per operation and per metric.

    uvicorn stops on Ctrl-C or SIGTERM and then raises the signal again under
    the handler it found. SIGTERM's handler here turns that into the same
    `KeyboardInterrupt` Ctrl-C raises, so either ends at the summary.
    """
    store = SpanStore()
    metrics = MetricStore()
    server = uvicorn.Server(
        uvicorn.Config(
            receiver(store, metrics),
            host=host,
            port=port,
            lifespan="off",
            log_level="warning",
        )
    )
    signal.signal(signal.SIGTERM, interrupt)
    print(
        f"collecting OTLP/HTTP spans at http://{host}:{port}{TRACES_PATH} and "
        f"metrics at http://{host}:{port}{METRICS_PATH}; Ctrl-C prints the summary",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.run()
    except KeyboardInterrupt:
        pass
    for timing in store.summary():
        print(timing.as_json_line())
    for metric in metrics.summary():
        print(metric.as_json_line())
    dropped = Collector(store, metrics).dropped()
    if dropped.any():
        print(dropped.as_json_line())
