"""Collect Rift's exported spans in memory and summarize them per operation.

`rift` built with `--features otlp` exports its `traced!`/`traced_async!` spans over
OTLP/HTTP, protobuf-encoded (`opentelemetry-otlp`'s `http-proto` feature), once
`OTEL_EXPORTER_OTLP_ENDPOINT` names a receiver. The collector here is that
receiver: it accepts `POST /v1/traces`, keeps each span's name and duration in
memory, and prints one JSON line per operation when it stops. Rift's macros name a
span after its operation, so grouping by span name groups by operation.
"""

from __future__ import annotations

import gzip
import json
import signal
import sys
import threading
from collections.abc import Iterator, Mapping
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

from google.protobuf.message import DecodeError
from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import (
    ExportTraceServiceRequest,
    ExportTraceServiceResponse,
)

TRACES_PATH = "/v1/traces"
PROTOBUF = "application/x-protobuf"


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
    """The durations of every span received, by operation, shared across requests."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.durations: dict[str, list[float]] = {}

    def record(self, body: bytes) -> None:
        """Decodes one export request and keeps its spans' durations."""
        request = ExportTraceServiceRequest.FromString(body)
        with self.lock:
            for name, duration in span_durations(request):
                self.durations.setdefault(name, []).append(duration)

    def summary(self) -> list[OperationTiming]:
        """The per-operation summary of every span received so far."""
        with self.lock:
            return summarize(
                {name: list(values) for name, values in self.durations.items()}
            )


def receiver(store: SpanStore) -> type[BaseHTTPRequestHandler]:
    """The request handler that feeds OTLP/HTTP export requests into `store`."""

    class Receiver(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            if self.path != TRACES_PATH:
                self.send_error(404, f"spans are exported to {TRACES_PATH}")
                return
            if self.headers.get("Content-Type", "").split(";")[0] != PROTOBUF:
                self.send_error(415, f"the collector reads {PROTOBUF}")
                return
            length = self.headers.get("Content-Length")
            if length is None:
                self.send_error(411, "an export request carries its Content-Length")
                return
            body = self.rfile.read(int(length))
            if self.headers.get("Content-Encoding") == "gzip":
                body = gzip.decompress(body)
            try:
                store.record(body)
            except DecodeError:
                self.send_error(400, "the body is not an ExportTraceServiceRequest")
                return
            response = ExportTraceServiceResponse().SerializeToString()
            self.send_response(200)
            self.send_header("Content-Type", PROTOBUF)
            self.send_header("Content-Length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)

        def log_message(self, format: str, *args: Any) -> None:
            """Keeps the terminal for the summary; one line per export is noise."""

    return Receiver


def interrupt(*_: object) -> None:
    """Ends the collector on SIGTERM the way Ctrl-C does, so the summary prints."""
    raise KeyboardInterrupt


def collect(host: str, port: int) -> None:
    """Serves until interrupted, then prints one JSON line per operation."""
    store = SpanStore()
    server = ThreadingHTTPServer((host, port), receiver(store))
    signal.signal(signal.SIGTERM, interrupt)
    print(
        f"collecting OTLP/HTTP spans at http://{host}:{server.server_port}{TRACES_PATH}; "
        "Ctrl-C prints the summary",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    for timing in store.summary():
        print(timing.as_json_line())
