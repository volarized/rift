"""Collect Rift's exported spans and metric points in memory, summarize them, and select them by time.

`rift` exports its `traced!` spans and its metrics over OTLP/HTTP, protobuf-encoded
(`opentelemetry-otlp`'s `http-proto` feature), once `OTEL_EXPORTER_OTLP_ENDPOINT` names a
receiver. The exporter appends `/v1/traces` and
`/v1/metrics` to that base URL. The collector here is that receiver. It accepts
`POST /v1/traces` and `POST /v1/metrics` and keeps, in memory:

- every span received, with its name, trace and span identifiers, start and end,
  attributes, and the `service.instance.id` of the process that sent it, at most
  `SPANS_MAX`, and its duration for the per-operation summary;
- every metric data point received, with its instrument's name, kind, and unit, its
  attributes and resource attributes, the `service.instance.id` of the process that sent
  it, its value (a histogram's count, sum, and buckets),
  `start_time_unix_nano`, `time_unix_nano`, and aggregation temporality, at most
  `POINTS_MAX`;
- the latest value of each metric series, for the summary `rift-dev trace-collector`
  prints when it stops.

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
import signal
import socket
import sys
import threading
import time
import zlib
from collections import deque
from collections.abc import Callable, Iterable, Iterator, Mapping
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta

import uvicorn
from google.protobuf.message import DecodeError
from opentelemetry.proto.collector.metrics.v1.metrics_service_pb2 import (
    ExportMetricsServiceRequest,
    ExportMetricsServiceResponse,
)
from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import (
    ExportTraceServiceRequest,
    ExportTraceServiceResponse,
)
from opentelemetry.proto.common.v1.common_pb2 import AnyValue, KeyValue
from opentelemetry.proto.metrics.v1.metrics_pb2 import (
    AGGREGATION_TEMPORALITY_CUMULATIVE,
    AGGREGATION_TEMPORALITY_DELTA,
    HistogramDataPoint,
    Metric,
    NumberDataPoint,
)
from starlette.applications import Starlette
from starlette.requests import ClientDisconnect, Request
from starlette.responses import PlainTextResponse, Response
from starlette.routing import Route

TRACES_PATH = "/v1/traces"
METRICS_PATH = "/v1/metrics"
PROTOBUF = "application/x-protobuf"
BODY_BYTES_MAX = 8 * 1024 * 1024
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
# The resource attribute that names the process a span or point came from.
INSTANCE_KEY = "service.instance.id"
# Attributes `tracing-opentelemetry` 0.34.0 puts on every span, which a span's line
# leaves out, as received from `rift` on 2026-10-05.
SPAN_KEYS_OMITTED = frozenset(["target", "busy_ns", "idle_ns"])
SPAN_KEY_PREFIXES_OMITTED = ("code.", "thread.")

Attributes = tuple[tuple[str, str], ...]


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
        }

    def any(self) -> bool:
        """Whether a bound refused or dropped anything."""
        return any(self.counts().values())

    def as_json_line(self) -> str:
        """These counts as one compact JSON object under `dropped`."""
        return json.dumps({"dropped": self.counts()})


def value_text(value: AnyValue) -> str:
    """An attribute value as text; a value of a kind the text does not read, such as an
    array, becomes its kind's name."""
    kind = value.WhichOneof("value")
    if kind == "bool_value":
        return str(value.bool_value).lower()
    if kind in ("string_value", "int_value", "double_value"):
        return str(getattr(value, kind))
    return str(kind)


def attribute_key(attributes: Iterable[KeyValue]) -> Attributes:
    """Attributes as a sorted, hashable key, each value as `value_text`."""
    return tuple(sorted((item.key, value_text(item.value)) for item in attributes))


def instance_of(resource: Attributes) -> str:
    """The `service.instance.id` resource attribute that names the sending process; empty
    when the resource carries none."""
    return dict(resource).get(INSTANCE_KEY, "")


def instance_text(instance: str) -> str:
    """The `service.instance.id=<id>  ` prefix of a printed line; empty without an id."""
    return f"{INSTANCE_KEY}={instance}  " if instance else ""


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
    stay apart.
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

    @property
    def instance(self) -> str:
        """The `service.instance.id` of the process that sent the point; empty when its
        resource carries none."""
        return instance_of(self.resource)

    def line(self) -> str:
        """The point as one line in the layout of a printed log record: time, kind,
        name, attributes, then the value.

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

    @property
    def duration_ms(self) -> float:
        """The span's duration in milliseconds."""
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

    def __init__(self, spans_max: int = SPANS_MAX) -> None:
        self.durations: dict[str, deque[float]] = {}
        self.spans: deque[SpanRecord] = deque(maxlen=spans_max)
        self.dropped = Dropped()
        self.received = 0
        self.kept_durations = 0
        self.lock = threading.Lock()

    def record(self, body: bytes) -> None:
        """Decodes one export request and keeps its spans."""
        request = ExportTraceServiceRequest.FromString(body)
        with self.lock:
            for resource_spans in request.resource_spans:
                instance = instance_of(
                    attribute_key(resource_spans.resource.attributes)
                )
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
                            )
                        )

    def keep(self, span: SpanRecord) -> None:
        """Keeps one span, dropping the oldest past a bound. The caller holds the lock."""
        self.received += 1
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
    """One metric name's series, each as its latest or accumulated value and count."""

    kind: str
    unit: str
    values: dict[Attributes, tuple[float, int]] = field(default_factory=dict)
    points: int = 0


class MetricStore:
    """Metric data points received: the newest `POINTS_MAX` whole, and the latest value
    of every series by metric name.

    For the latest value, a gauge and a cumulative sum or histogram replace a series'
    value with the newest data point; a delta sum or histogram adds each data point to
    it. The Rust exporter's default temporality is cumulative.
    """

    def __init__(self, points_max: int = POINTS_MAX) -> None:
        self.metrics: dict[str, MetricSeries] = {}
        self.points: deque[MetricPoint] = deque(maxlen=points_max)
        self.dropped = Dropped()
        self.received = 0
        self.lock = threading.Lock()

    def record(self, body: bytes) -> None:
        """Decodes one export request and keeps its data points."""
        request = ExportMetricsServiceRequest.FromString(body)
        with self.lock:
            for resource_metrics in request.resource_metrics:
                resource = attribute_key(resource_metrics.resource.attributes)
                for scope_metrics in resource_metrics.scope_metrics:
                    for metric in scope_metrics.metrics:
                        self.keep(metric, resource)

    def keep(self, metric: Metric, resource: Attributes = ()) -> None:
        """Keeps the data points of one metric of a supported kind; the caller holds the
        lock. A summary or exponential histogram is counted under `kinds`."""
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
        if held is None:
            if len(self.metrics) >= METRICS_MAX:
                self.dropped.metric_names += 1
                return
            held = MetricSeries(which, metric.unit)
            self.metrics[name] = held
        for point in points:
            key = attribute_key(point.attributes)
            if key not in held.values and len(held.values) >= SERIES_MAX:
                self.dropped.series += 1
                continue
            if isinstance(point, HistogramDataPoint):
                value, count = float(point.sum), int(point.count)
                kept = MetricPoint(
                    name,
                    which,
                    metric.unit,
                    temporality,
                    key,
                    resource,
                    point.start_time_unix_nano,
                    point.time_unix_nano,
                    value,
                    count,
                    tuple(point.explicit_bounds),
                    tuple(point.bucket_counts),
                )
            else:
                value, count = number_value(point), 0
                kept = MetricPoint(
                    name,
                    which,
                    metric.unit,
                    temporality,
                    key,
                    resource,
                    point.start_time_unix_nano,
                    point.time_unix_nano,
                    value,
                )
            self.received += 1
            if len(self.points) == self.points.maxlen:
                self.dropped.points += 1
            self.points.append(kept)
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


async def bounded_body(request: Request) -> bytes | None:
    """The encoded request body; None once it passes `BODY_BYTES_MAX`, read no further.

    A declared `content-length` past the bound refuses the body before any of it is
    read.
    """
    declared = request.headers.get("content-length", "")
    if declared.isdigit() and int(declared) > BODY_BYTES_MAX:
        return None
    chunks: list[bytes] = []
    size = 0
    async for chunk in request.stream():
        size += len(chunk)
        if size > BODY_BYTES_MAX:
            return None
        chunks.append(chunk)
    return b"".join(chunks)


def receiver(spans: SpanStore, metrics: MetricStore | None = None) -> Starlette:
    """The application that feeds OTLP/HTTP export requests into `spans` and `metrics`.

    Starlette answers any other path with 404 and any other method with 405.
    """
    held = metrics if metrics is not None else MetricStore()

    def route(
        path: str,
        keep: Callable[[bytes], None],
        response: bytes,
        noun: str,
    ) -> Route:
        async def export(request: Request) -> Response:
            if request.headers.get("content-type", "").split(";")[0] != PROTOBUF:
                return PlainTextResponse(f"the collector reads {PROTOBUF}", 415)
            try:
                encoded = await bounded_body(request)
                body = (
                    None
                    if encoded is None
                    else inflate(encoded, request.headers.get("content-encoding"))
                )
            except zlib.error:
                return PlainTextResponse("the body is not gzip", 400)
            except ClientDisconnect:
                return PlainTextResponse("the exporter disconnected", 400)
            if body is None:
                with held.lock:
                    held.dropped.bodies += 1
                return PlainTextResponse(
                    f"a body is at most {BODY_BYTES_MAX} bytes, encoded and decoded",
                    413,
                )
            try:
                keep(body)
            except DecodeError:
                return PlainTextResponse(f"the body is not an {noun}", 400)
            return Response(response, media_type=PROTOBUF)

        return Route(path, export, methods=["POST"])

    return Starlette(
        routes=[
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
        ]
    )


@dataclass(slots=True)
class Collector:
    """The stores one collector fills and the base URL it receives at.

    `endpoint` is empty for stores no receiver serves.
    """

    spans: SpanStore = field(default_factory=SpanStore)
    metrics: MetricStore = field(default_factory=MetricStore)
    endpoint: str = ""

    def environment(self) -> dict[str, str]:
        """The variables that point a server under test at this collector.

        `OTEL_EXPORTER_OTLP_ENDPOINT` is the base URL: Rift installs export only when it
        is set (`crates/rift-tracing/src/otlp.rs`), and the exporter appends
        `/v1/metrics` and `/v1/traces`. `OTEL_METRIC_EXPORT_INTERVAL` is read by the
        async periodic reader and `OTEL_BSP_SCHEDULE_DELAY` by the batch span
        processor's default configuration, both in milliseconds
        (`opentelemetry_sdk` 0.33.0).
        """
        if not self.endpoint:
            return {}
        return {
            "OTEL_EXPORTER_OTLP_ENDPOINT": self.endpoint,
            "OTEL_METRIC_EXPORT_INTERVAL": str(EXPORT_INTERVAL_MS),
            "OTEL_BSP_SCHEDULE_DELAY": str(EXPORT_INTERVAL_MS),
        }

    def dropped(self) -> Dropped:
        """What the bounds of both stores refused or dropped."""
        with self.metrics.lock, self.spans.lock:
            metrics, spans = self.metrics.dropped, self.spans.dropped
            return Dropped(
                bodies=metrics.bodies + spans.bodies,
                metric_names=metrics.metric_names,
                series=metrics.series,
                points=metrics.points,
                kinds=metrics.kinds,
                spans=spans.spans,
                durations=spans.durations,
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


@contextmanager
def collector(
    points_max: int = POINTS_MAX, spans_max: int = SPANS_MAX
) -> Iterator[Collector]:
    """Serves a receiver on `127.0.0.1` from a thread until the block exits.

    The socket is bound before the thread starts, on a port the system picks, and the
    block starts once uvicorn accepts connections, within `COLLECTOR_START_SECONDS`.
    The exit asks uvicorn to stop and joins the thread within
    `COLLECTOR_STOP_SECONDS`; a thread still running then raises `RuntimeError`.
    """
    stores = Collector(SpanStore(spans_max), MetricStore(points_max))
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        listener.bind((LOOPBACK, 0))
        port = listener.getsockname()[1]
        server = uvicorn.Server(
            uvicorn.Config(
                receiver(stores.spans, stores.metrics),
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
