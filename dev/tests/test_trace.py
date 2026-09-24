"""Summarizing recorded `/api/v3/traces` documents needs no live Docker collector.

The fixtures below match the shape a Jaeger v2 collector actually returned when probed
directly over its documented `/api/v3/traces` HTTP gateway: `traceId`/`spanId` as hex
strings, `startTimeUnixNano`/`endTimeUnixNano` as decimal nanosecond strings, one
`resourceSpans` entry per matched span.
"""

from __future__ import annotations

from rift_dev import trace


def span(name: str, start_ns: int, duration_ms: float) -> dict:
    """One `resourceSpans` entry shaped like Jaeger v2's `/api/v3/traces` response."""
    end_ns = start_ns + int(duration_ms * 1_000_000)
    return {
        "resource": {
            "attributes": [
                {"key": "service.name", "value": {"stringValue": "rift"}},
            ]
        },
        "scopeSpans": [
            {
                "scope": {"name": "rift"},
                "spans": [
                    {
                        "traceId": f"{start_ns:032x}",
                        "spanId": f"{start_ns:016x}",
                        "name": name,
                        "kind": 1,
                        "startTimeUnixNano": str(start_ns),
                        "endTimeUnixNano": str(end_ns),
                    }
                ],
            }
        ],
    }


def document(*spans: dict) -> dict:
    """One `/api/v3/traces` streamed document wrapping `spans`."""
    return {"result": {"resourceSpans": list(spans)}}


def test_span_durations_by_operation_groups_by_span_name() -> None:
    documents = [
        document(
            span("index.build", 1_000, 10.0),
            span("package.index", 2_000, 20.0),
        ),
        document(span("index.build", 3_000, 30.0)),
    ]

    durations = trace.span_durations_by_operation(documents)

    assert durations == {
        "index.build": [10.0, 30.0],
        "package.index": [20.0],
    }


def test_summarize_computes_count_total_and_percentiles_per_operation() -> None:
    documents = [
        document(
            span("index.build", 1_000, 10.0),
            span("index.build", 2_000, 20.0),
            span("index.build", 3_000, 30.0),
        )
    ]

    summaries = trace.summarize(documents)

    assert len(summaries) == 1
    summary = summaries[0]
    assert summary.operation == "index.build"
    assert summary.count == 3
    assert summary.total_ms == 60.0
    assert summary.p50_ms == 20.0
    assert summary.p95_ms == 30.0
    assert summary.max_ms == 30.0


def test_summarize_orders_operations_by_total_duration_descending() -> None:
    documents = [
        document(
            span("package.index", 1_000, 5.0),
            span("index.build", 2_000, 100.0),
        )
    ]

    summaries = trace.summarize(documents)

    assert [summary.operation for summary in summaries] == [
        "index.build",
        "package.index",
    ]


def test_summarize_over_no_documents_is_empty() -> None:
    assert trace.summarize([]) == []


def test_operation_timing_as_json_line_round_trips_through_json() -> None:
    import json

    summary = trace.OperationTiming(
        operation="index.build",
        count=3,
        total_ms=60.0,
        p50_ms=20.0,
        p95_ms=30.0,
        max_ms=30.0,
    )

    decoded = json.loads(summary.as_json_line())

    assert decoded == {
        "operation": "index.build",
        "count": 3,
        "total_ms": 60.0,
        "p50_ms": 20.0,
        "p95_ms": 30.0,
        "max_ms": 30.0,
    }


def test_decode_documents_parses_multiple_concatenated_json_objects() -> None:
    body = (
        '{"result": {"resourceSpans": []}}\n'
        '{"result": {"resourceSpans": [{"scopeSpans": []}]}}'
    )

    documents = trace.decode_documents(body)

    assert documents == [
        {"result": {"resourceSpans": []}},
        {"result": {"resourceSpans": [{"scopeSpans": []}]}},
    ]


def test_decode_documents_over_blank_body_is_empty() -> None:
    assert trace.decode_documents("  \n  ") == []
