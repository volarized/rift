"""Collect Rift's exported spans and metrics in memory and summarize them.

`rift` built with `--features otlp` exports its `traced!` spans and its metric
values over OTLP/HTTP, protobuf-encoded (`opentelemetry-otlp`'s `http-proto`
feature), once `OTEL_EXPORTER_OTLP_ENDPOINT` names a receiver. The exporter appends
`/v1/traces` and `/v1/metrics` to that base URL. The collector here is that
receiver: it accepts `POST /v1/traces`, keeps each span's name and duration in
memory, accepts `POST /v1/metrics`, keeps each metric's latest value per series,
and prints one JSON line per operation and one per metric name when it stops.
Rift's macros name a span after its operation, so grouping by span name groups by
operation.

Bounds: a request body, encoded and decompressed, is at most `BODY_BYTES_MAX`
bytes; the store keeps at most `METRICS_MAX` metric names and `SERIES_MAX` series
per name. Whatever a bound refused is counted and printed as a last `dropped` line,
so a summary cannot describe an incomplete capture as complete. Span durations are
kept without a count bound.

The receiver is a Starlette application that uvicorn serves on one asyncio event
loop, so requests are handled one await at a time and the store needs no lock.
"""

from __future__ import annotations

import json
import signal
import sys
import zlib
from collections.abc import Callable, Iterable, Iterator, Mapping
from dataclasses import dataclass, field

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
from opentelemetry.proto.common.v1.common_pb2 import KeyValue
from opentelemetry.proto.metrics.v1.metrics_pb2 import (
    AGGREGATION_TEMPORALITY_DELTA,
    HistogramDataPoint,
    Metric,
    NumberDataPoint,
)
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import PlainTextResponse, Response
from starlette.routing import Route

TRACES_PATH = "/v1/traces"
METRICS_PATH = "/v1/metrics"
PROTOBUF = "application/x-protobuf"
BODY_BYTES_MAX = 8 * 1024 * 1024
METRICS_MAX = 512
SERIES_MAX = 256
GZIP_WINDOW = 31


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


def span_durations(request: ExportTraceServiceRequest) -> Iterator[tuple[str, float]]:
    """Every span in one export request, as its name and duration in milliseconds."""
    for resource_spans in request.resource_spans:
        for scope_spans in resource_spans.scope_spans:
            for span in scope_spans.spans:
                nanoseconds = span.end_time_unix_nano - span.start_time_unix_nano
                yield span.name, nanoseconds / 1_000_000


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
    ]
    summaries.sort(key=lambda summary: summary.total_ms, reverse=True)
    return summaries


class SpanStore:
    """The durations of every span received, by operation."""

    def __init__(self) -> None:
        self.durations: dict[str, list[float]] = {}

    def record(self, body: bytes) -> None:
        """Decodes one export request and keeps its spans' durations."""
        request = ExportTraceServiceRequest.FromString(body)
        for name, duration in span_durations(request):
            self.durations.setdefault(name, []).append(duration)

    def summary(self) -> list[OperationTiming]:
        """The per-operation summary of every span received so far."""
        return summarize(self.durations)


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
    values: dict[tuple[tuple[str, str], ...], tuple[float, int]] = field(
        default_factory=dict
    )
    points: int = 0


@dataclass(slots=True)
class Dropped:
    """What the bounds refused, by reason."""

    bodies: int = 0
    metric_names: int = 0
    series: int = 0

    def any(self) -> bool:
        """Whether a bound refused anything."""
        return bool(self.bodies or self.metric_names or self.series)

    def as_json_line(self) -> str:
        """These counts as one compact JSON object under `dropped`."""
        return json.dumps(
            {
                "dropped": {
                    "bodies": self.bodies,
                    "metric_names": self.metric_names,
                    "series": self.series,
                }
            }
        )


def attribute_key(attributes: Iterable[KeyValue]) -> tuple[tuple[str, str], ...]:
    """A data point's attributes as a sorted, hashable key; each value as text.

    A value of a kind the key does not read, such as an array, becomes its kind's name.
    """
    key = []
    for item in attributes:
        kind = item.value.WhichOneof("value")
        text = (
            str(getattr(item.value, kind)).lower()
            if kind in ("string_value", "bool_value", "int_value", "double_value")
            else str(kind)
        )
        key.append((item.key, text))
    return tuple(sorted(key))


def number_value(point: NumberDataPoint) -> float:
    """A number data point's value, whichever of `as_double` and `as_int` it set."""
    if point.WhichOneof("value") == "as_int":
        return float(point.as_int)
    return float(point.as_double)


class MetricStore:
    """The latest value of every metric series received, by metric name.

    A gauge and a cumulative sum or histogram replace a series' value with the
    newest data point. A delta sum or histogram adds each data point to it. The Rust
    exporter's default temporality is cumulative.
    """

    def __init__(self) -> None:
        self.metrics: dict[str, MetricSeries] = {}
        self.dropped = Dropped()

    def record(self, body: bytes) -> None:
        """Decodes one export request and keeps its data points."""
        request = ExportMetricsServiceRequest.FromString(body)
        for resource_metrics in request.resource_metrics:
            for scope_metrics in resource_metrics.scope_metrics:
                for metric in scope_metrics.metrics:
                    self.keep(metric)

    def keep(self, metric: Metric) -> None:
        """Keeps the data points of one metric of a supported kind."""
        which = metric.WhichOneof("data")
        if which == "gauge":
            points, cumulative = metric.gauge.data_points, True
        elif which == "sum":
            points = metric.sum.data_points
            cumulative = (
                metric.sum.aggregation_temporality != AGGREGATION_TEMPORALITY_DELTA
            )
        elif which == "histogram":
            points = metric.histogram.data_points
            cumulative = (
                metric.histogram.aggregation_temporality
                != AGGREGATION_TEMPORALITY_DELTA
            )
        else:
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
            else:
                value, count = number_value(point), 0
            held.points += 1
            previous = held.values.get(key, (0.0, 0))
            held.values[key] = (
                (value, count)
                if cumulative
                else (previous[0] + value, previous[1] + count)
            )

    def summary(self) -> list[MetricSummary]:
        """One [`MetricSummary`] per metric name, ordered by name."""
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
                body = inflate(
                    await request.body(), request.headers.get("content-encoding")
                )
            except zlib.error:
                return PlainTextResponse("the body is not gzip", 400)
            if body is None:
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
    if metrics.dropped.any():
        print(metrics.dropped.as_json_line())
