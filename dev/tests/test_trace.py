"""The collector keeps what rift's OTLP exporter sends, within stated bounds.

The request bodies here are built with the protobuf classes the collector decodes
with, not captured from the Rust exporter. They exercise the handler, the bounds, the
summary, the time selection, and the served receiver's start and stop; the exporter's
own encoding is not exercised here.
"""

from __future__ import annotations

import asyncio
import gzip
import json
import socket
import threading
import urllib.error
import urllib.request
from collections.abc import MutableMapping
from typing import Any

import pytest
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
)
from rift_dev import trace
from rift_dev.trace import (
    BODY_BYTES_MAX,
    METRICS_MAX,
    METRICS_PATH,
    SERIES_MAX,
    TRACES_PATH,
    Collector,
    MetricStore,
    SpanStore,
    collector,
    receiver,
)


def post(
    app: Any,
    path: str,
    body: bytes,
    *,
    content_type: str = trace.PROTOBUF,
    encoding: str | None = None,
    method: str = "POST",
    headers: dict[str, str] | None = None,
) -> int:
    """One request through the ASGI application; the status it answered."""
    sent_headers = [(b"content-type", content_type.encode())]
    if encoding is not None:
        sent_headers.append((b"content-encoding", encoding.encode()))
    for key, value in (headers or {}).items():
        sent_headers.append((key.encode(), value.encode()))
    scope = {
        "type": "http",
        "asgi": {"version": "3.0"},
        "http_version": "1.1",
        "method": method,
        "path": path,
        "raw_path": path.encode(),
        "query_string": b"",
        "headers": sent_headers,
        "scheme": "http",
        "server": ("collector", 4318),
        "client": ("exporter", 1),
        "root_path": "",
    }
    status: list[int] = []
    sent = [False]

    async def receive() -> MutableMapping[str, Any]:
        if sent[0]:
            return {"type": "http.disconnect"}
        sent[0] = True
        return {"type": "http.request", "body": body, "more_body": False}

    async def send(message: MutableMapping[str, Any]) -> None:
        if message["type"] == "http.response.start":
            status.append(message["status"])

    asyncio.run(app(scope, receive, send))
    return status[0]


def metric_request(*metrics: Any) -> bytes:
    request = ExportMetricsServiceRequest()
    scope = request.resource_metrics.add().scope_metrics.add()
    for build in metrics:
        build(scope.metrics.add())
    return request.SerializeToString()


def counter(name: str, values: list[tuple[int, str]], delta: bool = False) -> Any:
    def build(metric: Any) -> None:
        metric.name = name
        metric.unit = "1"
        metric.sum.aggregation_temporality = (
            AGGREGATION_TEMPORALITY_DELTA
            if delta
            else AGGREGATION_TEMPORALITY_CUMULATIVE
        )
        for value, label in values:
            point = metric.sum.data_points.add()
            point.as_double = float(value)
            point.attributes.append(
                KeyValue(key="outcome", value=AnyValue(string_value=label))
            )

    return build


def histogram(name: str, count: int, total: float) -> Any:
    def build(metric: Any) -> None:
        metric.name = name
        metric.unit = "ms"
        metric.histogram.aggregation_temporality = AGGREGATION_TEMPORALITY_CUMULATIVE
        point = metric.histogram.data_points.add()
        point.count = count
        point.sum = total

    return build


def gauge(name: str, value: int) -> Any:
    def build(metric: Any) -> None:
        metric.name = name
        metric.unit = "By"
        point = metric.gauge.data_points.add()
        point.as_int = value

    return build


def summaries(store: MetricStore) -> dict[str, dict[str, Any]]:
    return {
        line["metric"]: line
        for line in (json.loads(item.as_json_line()) for item in store.summary())
    }


def test_the_metrics_path_keeps_the_latest_value_per_series() -> None:
    spans, metrics = SpanStore(), MetricStore()
    app = receiver(spans, metrics)
    first = metric_request(
        counter("rift.requests", [(3, "ok"), (1, "error")]),
        histogram("rift.elapsed", 2, 30.0),
        gauge("rift.process.memory", 1000),
    )
    second = metric_request(
        counter("rift.requests", [(5, "ok")]), histogram("rift.elapsed", 4, 90.0)
    )
    assert post(app, METRICS_PATH, first) == 200
    assert post(app, METRICS_PATH, second, encoding=None) == 200
    found = summaries(metrics)
    assert found["rift.requests"] == {
        "metric": "rift.requests",
        "kind": "sum",
        "unit": "1",
        "series": 2,
        "points": 3,
        "value": 6.0,
    }
    assert found["rift.elapsed"]["count"] == 4
    assert found["rift.elapsed"]["value"] == 90.0
    assert found["rift.process.memory"]["kind"] == "gauge"
    assert found["rift.process.memory"]["value"] == 1000.0
    assert not spans.summary()


def test_a_delta_sum_adds_each_data_point() -> None:
    metrics = MetricStore()
    app = receiver(SpanStore(), metrics)
    for value in (2, 3):
        body = metric_request(counter("rift.requests", [(value, "ok")], delta=True))
        assert post(app, METRICS_PATH, body) == 200
    assert summaries(metrics)["rift.requests"]["value"] == 5.0


def test_a_gzip_body_is_read_and_a_bad_one_is_refused() -> None:
    metrics = MetricStore()
    app = receiver(SpanStore(), metrics)
    body = metric_request(gauge("rift.process.memory", 7))
    assert post(app, METRICS_PATH, gzip.compress(body), encoding="gzip") == 200
    assert summaries(metrics)["rift.process.memory"]["value"] == 7.0
    assert post(app, METRICS_PATH, b"not gzip", encoding="gzip") == 400
    assert post(app, METRICS_PATH, b"\xff\xff\xff") == 400
    assert post(app, METRICS_PATH, body, content_type="application/json") == 415
    assert post(app, METRICS_PATH, body, method="GET") == 405
    assert post(app, "/v1/logs", body) == 404


def test_the_traces_path_still_keeps_span_durations() -> None:
    spans = SpanStore()
    app = receiver(spans, MetricStore())
    request = ExportTraceServiceRequest()
    span = request.resource_spans.add().scope_spans.add().spans.add()
    span.name = "index.build"
    span.start_time_unix_nano = 1_000_000
    span.end_time_unix_nano = 4_000_000
    assert post(app, TRACES_PATH, request.SerializeToString()) == 200
    assert {name: list(values) for name, values in spans.durations.items()} == {
        "index.build": [3.0]
    }


def test_a_body_past_the_bound_is_refused_and_counted(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(trace, "BODY_BYTES_MAX", 64)
    metrics = MetricStore()
    app = receiver(SpanStore(), metrics)
    assert post(app, METRICS_PATH, b"x" * 65) == 413
    inflated = gzip.compress(b"\x00" * 1000)
    assert len(inflated) < 64
    assert post(app, METRICS_PATH, inflated, encoding="gzip") == 413
    assert metrics.dropped.bodies == 2
    assert json.loads(metrics.dropped.as_json_line()) == {
        "dropped": {
            "bodies": 2,
            "metric_names": 0,
            "series": 0,
            "points": 0,
            "kinds": 0,
            "spans": 0,
            "durations": 0,
        }
    }
    assert BODY_BYTES_MAX == 8 * 1024 * 1024


def test_names_and_series_past_their_bounds_are_counted() -> None:
    metrics = MetricStore()
    names = [gauge(f"rift.metric.{n}", n) for n in range(METRICS_MAX + 3)]
    for start in range(0, len(names), 100):
        metrics.record(metric_request(*names[start : start + 100]))
    assert len(metrics.metrics) == METRICS_MAX
    assert metrics.dropped.metric_names == 3
    metrics = MetricStore()
    series = [(n, f"label-{n}") for n in range(SERIES_MAX + 2)]
    metrics.record(metric_request(counter("rift.requests", series)))
    assert summaries(metrics)["rift.requests"]["series"] == SERIES_MAX
    assert metrics.dropped.series == 2
    assert metrics.dropped.any()


SECOND = 1_000_000_000
# 2026-10-05T09:00:00Z in Unix nanoseconds.
NINE = 1_791_190_800 * SECOND


def timed_request(
    points: list[tuple[str, int, float]], *, service: str = "rift"
) -> bytes:
    """One request of cumulative sums, each `(name, time_unix_nano, value)`."""
    request = ExportMetricsServiceRequest()
    resource = request.resource_metrics.add()
    resource.resource.attributes.append(
        KeyValue(key="service.name", value=AnyValue(string_value=service))
    )
    scope = resource.scope_metrics.add()
    for name, at, value in points:
        metric = scope.metrics.add()
        metric.name = name
        metric.unit = "1"
        metric.sum.aggregation_temporality = AGGREGATION_TEMPORALITY_CUMULATIVE
        point = metric.sum.data_points.add()
        point.as_double = value
        point.start_time_unix_nano = NINE
        point.time_unix_nano = at
        point.attributes.append(
            KeyValue(key="outcome", value=AnyValue(string_value="ok"))
        )
    return request.SerializeToString()


def send(endpoint: str, path: str, body: bytes, length: int | None = None) -> int:
    """POST `body` to the served collector; the status it answered."""
    headers = {"content-type": trace.PROTOBUF}
    if length is not None:
        headers["content-length"] = str(length)
    request = urllib.request.Request(
        endpoint + path, data=body, headers=headers, method="POST"
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as answer:
            if path == METRICS_PATH:
                ExportMetricsServiceResponse.FromString(answer.read())
            else:
                ExportTraceServiceResponse.FromString(answer.read())
            return answer.status
    except urllib.error.HTTPError as error:
        return error.code


def test_a_served_collector_keeps_points_refuses_bad_bodies_and_leaves_nothing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    with collector() as served:
        endpoint = served.endpoint
        port = int(endpoint.rsplit(":", 1)[1])
        assert endpoint == f"http://127.0.0.1:{port}"
        histogram_request = ExportMetricsServiceRequest()
        metric = (
            histogram_request.resource_metrics.add().scope_metrics.add().metrics.add()
        )
        metric.name = "rift.mcp.request.duration"
        metric.unit = "s"
        metric.histogram.aggregation_temporality = AGGREGATION_TEMPORALITY_DELTA
        point = metric.histogram.data_points.add()
        point.count, point.sum = 3, 0.25
        point.explicit_bounds.extend([0.1, 1.0])
        point.bucket_counts.extend([2, 1, 0])
        point.start_time_unix_nano, point.time_unix_nano = NINE, NINE + SECOND
        assert send(endpoint, METRICS_PATH, timed_request([("rift.a", NINE, 2)])) == 200
        assert (
            send(endpoint, METRICS_PATH, histogram_request.SerializeToString()) == 200
        )
        span_request = ExportTraceServiceRequest()
        span = span_request.resource_spans.add().scope_spans.add().spans.add()
        span.name, span.trace_id, span.span_id = (
            "mcp.request",
            b"\x01" * 16,
            b"\x02" * 8,
        )
        span.start_time_unix_nano, span.end_time_unix_nano = NINE, NINE + 5_000_000
        span.attributes.append(
            KeyValue(key="request_id", value=AnyValue(string_value="7"))
        )
        assert send(endpoint, TRACES_PATH, span_request.SerializeToString()) == 200
        assert send(endpoint, METRICS_PATH, b"\xff\xff\xff") == 400
        monkeypatch.setattr(trace, "BODY_BYTES_MAX", 64)
        assert send(endpoint, METRICS_PATH, b"x" * 65) == 413
        assert send(endpoint, TRACES_PATH, b"\xff\xff\xff", length=3) == 400
        monkeypatch.undo()
        assert send(endpoint, METRICS_PATH, timed_request([("rift.b", NINE, 1)])) == 200
        kept = served.metrics.between(None, None)
        assert [point.name for point in kept] == [
            "rift.a",
            "rift.b",
            "rift.mcp.request.duration",
        ]
        assert kept[0].resource == (("service.name", "rift"),)
        assert kept[0].attributes == (("outcome", "ok"),)
        assert kept[0].temporality == "cumulative"
        histogram_point = kept[2]
        assert (histogram_point.kind, histogram_point.temporality) == (
            "histogram",
            "delta",
        )
        assert (histogram_point.count, histogram_point.value) == (3, 0.25)
        assert histogram_point.bounds == (0.1, 1.0)
        assert histogram_point.bucket_counts == (2, 1, 0)
        assert histogram_point.start_time_unix_nano == NINE
        (received,) = served.spans.between(None, None)
        assert received.request_id == "7"
        assert (received.trace_id, received.span_id) == ("01" * 16, "02" * 8)
        assert served.dropped().bodies == 1
    assert not [
        thread
        for thread in threading.enumerate()
        if thread.name == "rift-otlp-collector"
    ]
    with pytest.raises(ConnectionRefusedError):
        socket.create_connection(("127.0.0.1", port), timeout=1).close()


def test_the_stores_drop_the_oldest_past_their_bounds_and_count_it() -> None:
    metrics = MetricStore(points_max=3)
    metrics.record(
        timed_request([(f"rift.{n}", NINE + n * SECOND, n) for n in range(5)])
    )
    assert [point.name for point in metrics.between(None, None)] == [
        "rift.2",
        "rift.3",
        "rift.4",
    ]
    assert (metrics.received, metrics.dropped.points) == (5, 2)
    spans = SpanStore(spans_max=2)
    request = ExportTraceServiceRequest()
    scope = request.resource_spans.add().scope_spans.add()
    for n in range(3):
        span = scope.spans.add()
        span.name = f"op.{n}"
        span.end_time_unix_nano = NINE + n
    spans.record(request.SerializeToString())
    assert [span.name for span in spans.between(None, None)] == ["op.1", "op.2"]
    assert (spans.received, spans.dropped.spans) == (3, 1)
    assert [timing.operation for timing in spans.summary()] == ["op.0", "op.1", "op.2"]


def test_a_kind_the_store_does_not_read_is_counted() -> None:
    metrics = MetricStore()
    request = ExportMetricsServiceRequest()
    metric = request.resource_metrics.add().scope_metrics.add().metrics.add()
    metric.name = "rift.summary"
    metric.summary.data_points.add().count = 1
    metrics.record(request.SerializeToString())
    assert (metrics.received, metrics.dropped.kinds) == (0, 1)


def test_a_declared_length_past_the_bound_is_refused_before_the_body_is_read(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(trace, "BODY_BYTES_MAX", 64)
    metrics = MetricStore()
    app = receiver(SpanStore(), metrics)
    assert post(app, METRICS_PATH, b"", headers={"content-length": "65"}) == 413
    assert metrics.dropped.bodies == 1


def test_points_are_selected_by_time_and_instrument() -> None:
    held = Collector()
    held.metrics.record(
        timed_request(
            [
                ("rift.a", NINE - 1, 1),
                ("rift.a", NINE, 2),
                ("rift.b", NINE + SECOND, 3),
                ("rift.a", NINE + 10 * SECOND, 4),
            ]
        )
    )
    since = "2026-10-05T09:00:00.000+00:00"
    until = "2026-10-05T09:00:10.000+00:00"
    assert [point.value for point in held.points("rift.a", since, until)] == [2.0]
    assert [point.value for point in held.points("rift.a")] == [1.0, 2.0, 4.0]
    assert held.points("rift.c", since, until) == []
    assert trace.nanoseconds(since) == NINE


def test_the_environment_names_the_base_url_and_the_intervals() -> None:
    assert Collector().environment() == {}
    served = Collector(endpoint="http://127.0.0.1:4318")
    assert served.environment() == {
        "OTEL_EXPORTER_OTLP_ENDPOINT": "http://127.0.0.1:4318",
        "OTEL_METRIC_EXPORT_INTERVAL": "1000",
        "OTEL_BSP_SCHEDULE_DELAY": "1000",
    }
