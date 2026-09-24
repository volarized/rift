"""The in-memory collector reads what Rift's `http-proto` exporter sends.

Each request below is an `ExportTraceServiceRequest` built with the OTLP protobuf
classes and posted over HTTP, the shape `opentelemetry-otlp` writes to
`/v1/traces`; no Docker or external collector is involved.
"""

from __future__ import annotations

import gzip
import threading
import urllib.error
import urllib.request
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import ThreadingHTTPServer

import pytest
from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import (
    ExportTraceServiceRequest,
    ExportTraceServiceResponse,
)
from opentelemetry.proto.trace.v1.trace_pb2 import ResourceSpans, ScopeSpans, Span
from rift_dev import trace


def request(*spans: tuple[str, int, float]) -> ExportTraceServiceRequest:
    """One export request carrying each span as its name, start, and duration."""
    return ExportTraceServiceRequest(
        resource_spans=[
            ResourceSpans(
                scope_spans=[
                    ScopeSpans(
                        spans=[
                            Span(
                                name=name,
                                start_time_unix_nano=start_ns,
                                end_time_unix_nano=start_ns + int(duration_ms * 1e6),
                            )
                            for name, start_ns, duration_ms in spans
                        ]
                    )
                ]
            )
        ]
    )


@contextmanager
def collector() -> Iterator[tuple[str, trace.SpanStore]]:
    """A collector serving on an ephemeral loopback port, and the store it feeds."""
    store = trace.SpanStore()
    server = ThreadingHTTPServer(("127.0.0.1", 0), trace.receiver(store))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", store
    finally:
        server.shutdown()
        server.server_close()


def post(
    url: str,
    body: bytes,
    content_type: str = trace.PROTOBUF,
    encoding: str | None = None,
) -> tuple[int, bytes]:
    headers = {"Content-Type": content_type}
    if encoding:
        headers["Content-Encoding"] = encoding
    call = urllib.request.Request(url, data=body, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(call, timeout=5) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def test_exported_spans_are_summarized_per_operation() -> None:
    with collector() as (url, store):
        status, body = post(
            f"{url}/v1/traces",
            request(
                ("index.populate", 0, 30.0),
                ("index.populate", 10, 10.0),
                ("search", 20, 5.0),
            ).SerializeToString(),
        )
        assert status == 200
        ExportTraceServiceResponse.FromString(body)
        summary = store.summary()
    assert [timing.operation for timing in summary] == ["index.populate", "search"]
    populate = summary[0]
    assert populate.count == 2
    assert populate.total_ms == pytest.approx(40.0)
    assert populate.max_ms == pytest.approx(30.0)


def test_spans_accumulate_across_requests_and_gzip_bodies() -> None:
    with collector() as (url, store):
        post(f"{url}/v1/traces", request(("search", 0, 1.0)).SerializeToString())
        compressed = gzip.compress(request(("search", 5, 3.0)).SerializeToString())
        status, _ = post(f"{url}/v1/traces", compressed, encoding="gzip")
        assert status == 200
        [search] = store.summary()
    assert search.count == 2
    assert search.total_ms == pytest.approx(4.0)


@pytest.mark.parametrize(
    "path,content_type,body,status",
    [
        ("/v1/metrics", trace.PROTOBUF, b"", 404),
        ("/v1/traces", "application/json", b"{}", 415),
        ("/v1/traces", trace.PROTOBUF, b"\xff\xff\xff", 400),
    ],
)
def test_a_request_the_collector_cannot_read_is_refused(
    path: str, content_type: str, body: bytes, status: int
) -> None:
    with collector() as (url, store):
        assert post(f"{url}{path}", body, content_type)[0] == status
        assert store.summary() == []


def test_percentile_uses_nearest_rank() -> None:
    values = [float(value) for value in range(1, 101)]
    assert trace.percentile(values, 0.50) == 51.0
    assert trace.percentile(values, 0.95) == 96.0
    assert trace.percentile([], 0.50) == 0.0


def test_a_timing_prints_as_one_json_line() -> None:
    [timing] = trace.summarize({"search": [1.23456]})
    assert timing.as_json_line() == (
        '{"operation": "search", "count": 1, "total_ms": 1.235, '
        '"p50_ms": 1.235, "p95_ms": 1.235, "max_ms": 1.235}'
    )
