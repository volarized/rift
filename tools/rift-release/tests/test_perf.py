"""Pure parser tests for the repository's edit measurement reports."""

from __future__ import annotations

import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

# The repository's pytest suite also exercises dev's dependency-free report logic.
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "dev/src"))
from rift_dev.perf.profile import Frame, merged_tree, on_cpu, parse_sample
from rift_dev.perf.stages import (
    Scope,
    Stage,
    duration_seconds,
    parse_stages,
    render_table,
    stage_table,
)

WRITTEN = datetime(2026, 11, 19, 10, tzinfo=timezone.utc)  # noqa: UP017 - release tests support Python 3.10.


@pytest.mark.parametrize(
    "text, expected",
    [
        ("0ns", 0),
        ("12.5ns", 12.5e-9),
        ("4µs", 4e-6),
        ("4μs", 4e-6),
        ("4us", 4e-6),
        ("200ms", 0.2),
        ("1.25s", 1.25),
    ],
)
def test_trace_duration_preserves_units(text: str, expected: float) -> None:
    assert duration_seconds(text) == pytest.approx(expected)


@pytest.mark.parametrize(
    "text", ["", "1", "-1ms", "NaNs", "1.2s trailing", "1m", "1,5ms"]
)
def test_trace_duration_rejects_incomplete_values(text: str) -> None:
    with pytest.raises(ValueError, match="expected a trace duration"):
        duration_seconds(text)


@pytest.mark.parametrize("ending", ["\n", "\r\n", ""])
def test_span_parser_separates_index_request_background_and_keeps_idle(
    ending: str,
) -> None:
    contexts = [
        "component=index epoch=4 trigger=filesystem",
        "component=mcp operation=tools/call tool=facets",
        "component=worker operation=worker.run work=background workspace validation",
    ]
    text = (
        ending.join(
            [
                "not a log record",
                *[
                    f"2026-11-19 10:00:00.500Z INFO  rift::test   {context}  ↳ fingerprint.fold component=index operation=fingerprint.fold close ✓ busy=12ms idle=3ms"
                    for context in contexts
                ],
                "2026-11-19 10:00:00.600Z INFO  rift::test   component=index epoch=4 trigger=filesystem  index snapshot published operation=index.publish",
            ]
        )
        if ending
        else "\n".join(
            [
                f"2026-11-19 10:00:00.500Z INFO  rift::test   {context}  ↳ fingerprint.fold component=index operation=fingerprint.fold close ✓ busy=12ms idle=3ms"
                for context in contexts
            ]
        )
    )
    stages = parse_stages(text)
    assert [stage.scope for stage in stages] == ["index", "request", "background"]
    assert [stage.seconds for stage in stages] == pytest.approx([0.015] * 3)
    assert stages[0].ended == WRITTEN + timedelta(seconds=0.5)


def span(operation: str, seconds: float, ended: float, scope: Scope = "index") -> Stage:
    return Stage(
        operation, scope, WRITTEN + timedelta(seconds=ended), seconds, 0, "", ""
    )


def test_stage_table_keeps_parent_child_and_identical_capture_scopes_separate() -> None:
    stages = [
        span("RebuildRequest::change_set_with_records", 0.01, 0.10),
        span("fingerprint.fold", 0.002, 0.2),
        span("fingerprint.fold", 9, 0.2, "request"),
        span("documentation.collect", 0.3, 0.8),
        span("collect_documentation_incremental", 0.25, 0.7),
        span("index.symbol_documents", 0.001, 0.81),
        span("index.visible_digests", 0.002, 0.82),
        span("fingerprint.capture", 0.4, 0.5, "request"),
        span("fingerprint.capture", 0.02, 1.2, "request"),
        span("fingerprint.discover", 0.2, 1.4, "background"),
        span("fingerprint.visible_paths", 0.3, 1.7, "background"),
    ]
    rows = stage_table(stages, WRITTEN, WRITTEN + timedelta(seconds=1))
    assert [row.number for row in rows] == list(range(1, 20))
    assert rows[0].seconds == pytest.approx(0.09)
    assert rows[6].seconds == 0.002
    assert rows[10].seconds == 0.3
    assert rows[11].seconds == 0.003
    assert rows[15].seconds == 0.5
    assert rows[16].seconds == 0.02
    assert rows[17].seconds is None
    assert rows[18].seconds is None
    assert "frame only" in rows[18].evidence
    table = render_table(rows)
    assert "| 7 | fingerprint.fold (index) | 0.002000 | F |" in table
    assert "n/a" in table
    assert "publication drop" in table


def test_absent_spans_are_unknown_including_a_lazy_map() -> None:
    rows = stage_table([], WRITTEN, WRITTEN)
    assert all(row.seconds is None for row in rows)
    assert rows[12].stage == "index.map"
    assert "untraced or absent" in render_table(rows)


SAMPLE = """Process: rift
Call graph:
    10 Thread_1
    + 10 root  (in rift) + 4  [0x1]
    + ! 6 work  (in rift)
    + ! : 2 sha2::compress
    + ! 4 __psynch_cvwait  (in libsystem_kernel.dylib)
Total number in stack: 10
    999 not_a_frame
"""


def test_sample_parser_excludes_summary_and_counts_only_on_cpu_self_ticks() -> None:
    frames = parse_sample(SAMPLE)
    assert len(frames) == 5
    total, counts = on_cpu(frames)
    assert total == 6
    assert counts == {"root": 6, "work": 6, "sha2::compress": 2}
    assert on_cpu(frames, self_only=True) == (6, {"work": 4, "sha2::compress": 2})
    assert on_cpu(frames, "sha2") == (2, {"root": 2, "work": 2, "sha2::compress": 2})
    assert on_cpu(frames, "missing") == (0, {})


def test_recursive_sample_frames_do_not_double_count_inclusive_ticks() -> None:
    frames = [Frame(0, 4, "Thread_1"), Frame(1, 4, "root"), Frame(2, 4, "root")]
    assert on_cpu(frames) == (4, {"root": 4})


def test_tree_merges_disjoint_roots_and_honors_depth() -> None:
    frames = parse_sample(SAMPLE) + parse_sample(SAMPLE)
    total, tree = merged_tree(frames, "root", 1)
    assert total == 20
    assert tree == {("work",): 12, ("__psynch_cvwait",): 8}
    total, tree = merged_tree(frames, "root", 2)
    assert tree[("work", "sha2::compress")] == 4
    assert merged_tree(frames, "missing", 2) == (0, {})


def test_sample_parser_rejects_other_profiler_formats() -> None:
    with pytest.raises(ValueError, match="Call graph"):
        parse_sample("perf record data is a binary format")


def test_table_excludes_later_noop_epochs_from_the_publication_path() -> None:
    rows = stage_table(
        [
            span("RebuildRequest::change_set_with_records", 0.01, 0.1),
            span("index.map", 0.2, 0.8),
            span("dependency.probe", 0.05, 0.9),
            span("index.lexical_write", 0.01, 0.99),
            span("RebuildRequest::change_set_with_records", 0.04, 1.5),
            span("index.map", 9, 2),
        ],
        WRITTEN,
        WRITTEN + timedelta(seconds=1),
    )
    assert rows[1].seconds == 0.01
    assert rows[12].seconds == 0.2
    assert rows[13].seconds == pytest.approx(0.18)
    assert rows[14].seconds == pytest.approx(0.02)


def test_request_capture_straddling_publication_reports_only_the_post_publication_part() -> (
    None
):
    rows = stage_table(
        [span("fingerprint.capture", 0.5, 1.1, "request")],
        WRITTEN,
        WRITTEN + timedelta(seconds=1),
    )
    assert rows[16].seconds == pytest.approx(0.1)
    assert rows[16].evidence == "span overlap after publication (busy + idle)"


def test_active_rust_poll_frame_is_not_classified_as_a_native_idle_wait() -> None:
    frames = [
        Frame(0, 7, "Thread_1"),
        Frame(1, 7, "root"),
        Frame(2, 3, "rift_index::poll_work"),
        Frame(2, 4, "poll$NOCANCEL"),
    ]
    assert on_cpu(frames) == (3, {"root": 3, "rift_index::poll_work": 3})
