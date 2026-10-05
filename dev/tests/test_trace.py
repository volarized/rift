"""The collector keeps what rift's OTLP exporter sends, within stated bounds.

The request bodies here are built with the protobuf classes the collector decodes
with, not captured from the Rust exporter. They exercise the handler, the bounds, and
the summary; the exporter's own encoding was not exercised end to end.
"""

from __future__ import annotations

import asyncio
import gzip
import json
from collections.abc import MutableMapping
from typing import Any

import pytest
from opentelemetry.proto.collector.metrics.v1.metrics_service_pb2 import (
    ExportMetricsServiceRequest,
)
from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import (
    ExportTraceServiceRequest,
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
    MetricStore,
    SpanStore,
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
) -> int:
    """One request through the ASGI application; the status it answered."""
    headers = [(b"content-type", content_type.encode())]
    if encoding is not None:
        headers.append((b"content-encoding", encoding.encode()))
    scope = {
        "type": "http",
        "asgi": {"version": "3.0"},
        "http_version": "1.1",
        "method": method,
        "path": path,
        "raw_path": path.encode(),
        "query_string": b"",
        "headers": headers,
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
    assert spans.durations == {"index.build": [3.0]}


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
        "dropped": {"bodies": 2, "metric_names": 0, "series": 0}
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
