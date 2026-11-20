"""Parse trace durations and report the edit's publication and read stages."""

from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Literal

from rift_dev.log_records import Line, fields, lines

Scope = Literal["index", "request", "background", "other"]
DURATION = re.compile(r"(\d+(?:\.\d+)?)(ns|µs|μs|us|ms|s)")
UNITS = {"ns": 1e-9, "µs": 1e-6, "μs": 1e-6, "us": 1e-6, "ms": 1e-3, "s": 1.0}


def duration_seconds(text: str) -> float:
    """Decode one tracing duration, rejecting absent units and incomplete values."""
    match = DURATION.fullmatch(text)
    if match is None:
        raise ValueError(f"expected a trace duration such as 12.5ms, observed {text!r}")
    return float(match[1]) * UNITS[match[2]]


@dataclass(frozen=True)
class Stage:
    operation: str
    scope: Scope
    ended: datetime
    busy_seconds: float
    idle_seconds: float
    context: str
    nested: str

    @property
    def seconds(self) -> float:
        return self.busy_seconds + self.idle_seconds


def scope(record: Line) -> Scope:
    """Separate identical operation names by their owning root span."""
    context = fields(record.context)
    if context.get("operation") == "tools/call":
        return "request"
    if context.get("work") == "background workspace validation":
        return "background"
    if context.get("component") == "index" and "epoch" in context:
        return "index"
    return "other"


def parse_stages(text: str) -> list[Stage]:
    """Keep span closes with their root context; parent and child spans stay separate."""
    return [
        Stage(
            row.operation or row.nested_name,
            scope(row),
            row.time,
            duration_seconds(row.label("busy")),
            duration_seconds(row.label("idle")),
            row.context,
            row.nested,
        )
        for row in lines(text)
        if row.closes()
    ]


@dataclass(frozen=True)
class StageRow:
    number: int
    stage: str
    scale: str
    seconds: float | None
    evidence: str


# These rows follow the publication/read sequence. Untraced frames stay unknown.
STAGES: tuple[tuple[str, str, tuple[str, ...]], ...] = (
    ("watcher + debounce", "E", ()),
    ("change_set_with_records", "E", ("RebuildRequest::change_set_with_records",)),
    ("IndexContents::carried_from", "F", ("IndexContents::carried_from",)),
    ("index.parse", "E", ("index.parse",)),
    ("index.frameworks", "F", ("index.frameworks",)),
    ("capture_build_paths", "F", ("IndexContents::capture_build_paths",)),
    ("fingerprint.fold (index)", "F", ("fingerprint.fold",)),
    (
        "prepare_captured_project_facts",
        "W",
        ("WorkspaceSemantics::prepare_captured_project_facts",),
    ),
    ("build_project_facts", "W", ("WorkspaceSemantics::build_project_facts",)),
    ("documentation.declarations", "W", ("documentation.declarations",)),
    ("documentation.collect", "W", ("documentation.collect",)),
    (
        "symbol_documents + visible_digests",
        "F",
        ("index.symbol_documents", "index.visible_digests"),
    ),
    ("index.map", "W", ("index.map",)),
    ("recommendations + dependency.probe", "F + process", ("dependency.probe",)),
    ("lexical_write + publish", "E", ("index.lexical_write",)),
    (
        "background validation walk",
        "F",
        (
            "fingerprint.discover",
            "fingerprint.read",
            "fingerprint.source_policy",
            "fingerprint.visible_paths",
            "fingerprint.visible_read",
        ),
    ),
    ("request capture after publication", "F", ("fingerprint.capture",)),
    (
        "build_symbol_declarations (first read)",
        "W",
        ("WorkspaceIndex::build_symbol_declarations", "build_symbol_declarations"),
    ),
    ("previous publication drop on request", "W", ()),
)


def stage_table(
    stages: list[Stage], written: datetime, published: datetime
) -> list[StageRow]:
    """Report observed spans, never substitute zero for missing instrumentation.

    Durations include busy and idle time. Rows may overlap; their sum is not wall
    time. Frame-only work requires a profiler rather than an inferred residual.
    """
    stages = [
        stage
        for stage in stages
        if stage.ended >= written
        and (stage.scope != "index" or stage.ended <= published)
    ]
    rows = []
    for number, (name, scale, operations) in enumerate(STAGES, 1):
        selected_scope = (
            "background" if number == 16 else "request" if number >= 17 else "index"
        )
        selected = [
            stage
            for stage in stages
            if stage.scope == selected_scope
            and stage.operation in operations
            and stage.ended >= written
            and (number != 17 or stage.ended >= published)
        ]
        seconds = sum(stage.seconds for stage in selected) if selected else None
        evidence = "span busy + idle" if selected else "untraced or absent"
        if number == 17 and selected:
            seconds = sum(
                (
                    stage.ended
                    - max(published, stage.ended - timedelta(seconds=stage.seconds))
                ).total_seconds()
                for stage in selected
            )
            evidence = "span overlap after publication (busy + idle)"
        if number == 1:
            changes = [
                stage
                for stage in stages
                if stage.scope == "index" and stage.operation == STAGES[1][2][0]
            ]
            if changes:
                first = min(changes, key=lambda stage: stage.ended)
                seconds = max(
                    0.0, (first.ended - written).total_seconds() - first.seconds
                )
                evidence = "write to first change-set start (includes scheduling)"
        lexical = [
            stage
            for stage in stages
            if stage.scope == "index" and stage.operation == "index.lexical_write"
        ]
        if number == 14 and selected:
            evidence = "dependency.probe only; recommendations untraced"
            maps = [
                stage
                for stage in stages
                if stage.scope == "index" and stage.operation == "index.map"
            ]
            if maps and lexical:
                seconds = max(
                    0.0,
                    (lexical[0].ended - maps[-1].ended).total_seconds()
                    - lexical[0].seconds,
                )
                evidence = "map end to lexical start (includes scheduling)"
        if number == 15 and lexical:
            seconds = max(
                0.0, (published - lexical[0].ended).total_seconds() + lexical[0].seconds
            )
            evidence = "lexical start to publication (includes scheduling)"
        if number in (18, 19) and not selected:
            evidence = "frame only; inspect sample"
        rows.append(StageRow(number, name, scale, seconds, evidence))
    return rows


def render_table(rows: list[StageRow]) -> str:
    """Render the stable stage order with seconds and explicit missing values."""
    output = [
        "| # | Stage | Seconds | Class | Evidence |",
        "| - | --- | ---: | --- | --- |",
    ]
    for row in rows:
        elapsed = "n/a" if row.seconds is None else f"{row.seconds:.6f}"
        output.append(
            f"| {row.number} | {row.stage} | {elapsed} | {row.scale} | {row.evidence} |"
        )
    return "\n".join(output)
