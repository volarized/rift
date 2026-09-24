"""Collect Rift's exported spans in memory and summarize them per operation.

`rift` built with `--features otlp` exports its `traced!`/`traced_async!` spans over
OTLP/HTTP, protobuf-encoded (`opentelemetry-otlp`'s `http-proto` feature), once
`OTEL_EXPORTER_OTLP_ENDPOINT` names a receiver. The collector here is that
receiver: it accepts `POST /v1/traces`, keeps each span's name and duration in
memory, and prints one JSON line per operation when it stops. Rift's macros name a
span after its operation, so grouping by span name groups by operation.

The receiver is a Starlette application that uvicorn serves on one asyncio event
loop, so requests are handled one await at a time and the store needs no lock.
"""

from __future__ import annotations

import gzip
import json
import signal
import sys
from collections.abc import Iterator, Mapping
from dataclasses import dataclass

import uvicorn
from google.protobuf.message import DecodeError
from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import (
    ExportTraceServiceRequest,
    ExportTraceServiceResponse,
)
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import PlainTextResponse, Response
from starlette.routing import Route

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


def receiver(store: SpanStore) -> Starlette:
    """The application that feeds OTLP/HTTP export requests into `store`.

    Starlette answers any other path with 404 and any other method with 405.
    """

    async def export(request: Request) -> Response:
        if request.headers.get("content-type", "").split(";")[0] != PROTOBUF:
            return PlainTextResponse(f"the collector reads {PROTOBUF}", 415)
        body = await request.body()
        if request.headers.get("content-encoding") == "gzip":
            body = gzip.decompress(body)
        try:
            store.record(body)
        except DecodeError:
            return PlainTextResponse(
                "the body is not an ExportTraceServiceRequest", 400
            )
        return Response(
            ExportTraceServiceResponse().SerializeToString(), media_type=PROTOBUF
        )

    return Starlette(routes=[Route(TRACES_PATH, export, methods=["POST"])])


def interrupt(*_: object) -> None:
    """Ends the collector on SIGTERM the way Ctrl-C does, so the summary prints."""
    raise KeyboardInterrupt


def collect(host: str, port: int) -> None:
    """Serves until interrupted, then prints one JSON line per operation.

    uvicorn stops on Ctrl-C or SIGTERM and then raises the signal again under
    the handler it found. SIGTERM's handler here turns that into the same
    `KeyboardInterrupt` Ctrl-C raises, so either ends at the summary.
    """
    store = SpanStore()
    server = uvicorn.Server(
        uvicorn.Config(
            receiver(store), host=host, port=port, lifespan="off", log_level="warning"
        )
    )
    signal.signal(signal.SIGTERM, interrupt)
    print(
        f"collecting OTLP/HTTP spans at http://{host}:{port}{TRACES_PATH}; "
        "Ctrl-C prints the summary",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.run()
    except KeyboardInterrupt:
        pass
    for timing in store.summary():
        print(timing.as_json_line())
