"""Summarize spans a local OTLP collector holds, one JSON line per operation.

Jaeger v2 serves its documented HTTP query gateway at `/api/v3/*`
(https://github.com/jaegertracing/jaeger-idl/blob/main/proto/api_v3/query_service.proto);
the pre-v2 `/api/traces` JSON API answers 404 on a v2 collector. `/api/v3/traces` streams
one or more JSON documents, each shaped `{"result": {"resourceSpans": [...]}}` in OTLP's
own JSON encoding - `traceId`/`spanId` as hex strings, `startTimeUnixNano` and
`endTimeUnixNano` as decimal nanosecond strings - so parsing walks that nested shape
rather than a Jaeger-specific one. `rift`'s own `traced!`/`traced_async!` macros name a
span after its `operation`, so grouping by a span's `name` field groups by operation.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta

REQUEST_TIMEOUT_SECONDS = 10.0


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


def decode_documents(body: str) -> list[dict]:
    """Parses one or more concatenated top-level JSON documents from a streamed body.

    The gRPC-gateway streaming endpoints write consecutive JSON values with no
    separator between them; a small result fits in one, so plain `json.loads` covers
    that case, and this handles a body carrying more than one without requiring one.
    """
    decoder = json.JSONDecoder()
    documents = []
    text = body.strip()
    index = 0
    while index < len(text):
        document, end = decoder.raw_decode(text, index)
        documents.append(document)
        index = end
        while index < len(text) and text[index].isspace():
            index += 1
    return documents


def span_durations_by_operation(documents: list[dict]) -> dict[str, list[float]]:
    """Every span's duration in milliseconds, keyed by its `name`.

    `documents` is [`decode_documents`]'s output: each entry carries
    `result.resourceSpans[].scopeSpans[].spans[]`, OTLP's nested span layout.
    """
    durations: dict[str, list[float]] = {}
    for document in documents:
        for resource_spans in document.get("result", {}).get("resourceSpans", []):
            for scope_spans in resource_spans.get("scopeSpans", []):
                for span in scope_spans.get("spans", []):
                    name = span.get("name")
                    if name is None:
                        continue
                    start = int(span["startTimeUnixNano"])
                    end = int(span["endTimeUnixNano"])
                    durations.setdefault(name, []).append((end - start) / 1_000_000)
    return durations


def percentile(values: list[float], fraction: float) -> float:
    """The `fraction`-th percentile of `values` by nearest-rank, ascending."""
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(fraction * len(ordered)))
    return ordered[index]


def summarize(documents: list[dict]) -> list[OperationTiming]:
    """One [`OperationTiming`] per operation name, highest total duration first."""
    by_operation = span_durations_by_operation(documents)
    summaries = [
        OperationTiming(
            operation=name,
            count=len(values),
            total_ms=sum(values),
            p50_ms=percentile(values, 0.50),
            p95_ms=percentile(values, 0.95),
            max_ms=max(values),
        )
        for name, values in by_operation.items()
    ]
    summaries.sort(key=lambda summary: summary.total_ms, reverse=True)
    return summaries


def fetch_traces(
    base_url: str, service: str, since: timedelta, search_depth: int
) -> list[dict]:
    """Every `/api/v3/traces` document the collector holds for `service` since `since` ago.

    A collector holding no matching trace answers HTTP 404 with an error body, not an
    empty result - observed directly against a Jaeger v2 collector - so that status
    means zero traces here, not a request failure.
    """
    now = datetime.now(UTC)
    query = urllib.parse.urlencode(
        {
            "query.service_name": service,
            "query.start_time_min": (now - since).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "query.start_time_max": now.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "query.search_depth": search_depth,
        }
    )
    url = f"{base_url.rstrip('/')}/api/v3/traces?{query}"
    try:
        with urllib.request.urlopen(url, timeout=REQUEST_TIMEOUT_SECONDS) as response:
            body = response.read().decode("utf-8")
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return []
        raise
    return decode_documents(body)


def main(base_url: str, service: str, since: timedelta, search_depth: int) -> None:
    """Fetches, summarizes, and prints one JSON line per operation to stdout."""
    documents = fetch_traces(base_url, service, since, search_depth)
    for summary in summarize(documents):
        print(summary.as_json_line())
