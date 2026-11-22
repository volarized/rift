"""Measure restored Bun edits and attribute project facts on Linux."""

from __future__ import annotations

import gzip
import json
import sys
from collections.abc import Iterable
from itertools import pairwise
from pathlib import Path

import psutil
from check_burst_pair import VARIANTS, EditCase, run
from rift_dev.corpus_burst import FILE_COUNT
from rift_dev.corpus_cache import pins

DECISION = "EdgeStore::finalize_static_adjacency"
PROJECT = "WorkspaceSemantics::build_project_facts"
SEMANTICS = "index.semantics"
PHASES = (
    "analysis.normalize_project",
    "analysis.graph_admission",
    "analysis.prepare_static_index",
    "analysis.resolve_static_files",
    "analysis.static_admission",
    "analysis.retain_static_occurrences",
)
ENCLOSING = "analysis.enclosing_definitions"


def child_span(parent: dict, spans: list[dict], name: str) -> dict:
    children = [
        span
        for span in spans
        if span["name"] == name
        and span["instance"] == parent["instance"]
        and span["trace_id"] == parent["trace_id"]
        and span.get("parent_span_id") == parent["span_id"]
    ]
    if len(children) != 1:
        raise ValueError(f"{parent['name']} has {len(children)} children named {name}")
    [child] = children
    if not (
        parent["start_time_unix_nano"]
        <= child["start_time_unix_nano"]
        < child["end_time_unix_nano"]
        <= parent["end_time_unix_nano"]
    ):
        raise ValueError(f"{name} falls outside its parent")
    return child


def phase_measurement(span: dict) -> dict:
    fields = dict(span["attributes"])
    busy, idle = int(fields["busy_ns"]), int(fields["idle_ns"])
    if min(busy, idle) < 0:
        raise ValueError(f"negative measurement for {span['name']}")
    return {
        "trace_id": span["trace_id"],
        "span_id": span["span_id"],
        "parent_span_id": span.get("parent_span_id"),
        "wall_ns": span["end_time_unix_nano"] - span["start_time_unix_nano"],
        "busy_ns": busy,
        "idle_ns": idle,
    }


def attribute_project(project: dict, spans: list[dict]) -> tuple[dict, dict]:
    [semantics] = [
        span
        for span in spans
        if span["name"] == SEMANTICS
        and span["instance"] == project["instance"]
        and span["trace_id"] == project["trace_id"]
        and span["span_id"] == project["parent_span_id"]
    ]
    child_span(semantics, [project], PROJECT)
    children = {name: child_span(project, spans, name) for name in PHASES}
    if any(
        left["end_time_unix_nano"] > right["start_time_unix_nano"]
        for left, right in pairwise(children.values())
    ):
        raise ValueError("project fact phases overlap or appear out of order")
    graph = children["analysis.graph_admission"]
    enclosing = child_span(graph, spans, ENCLOSING)
    span_ids = {
        span["span_id"] for span in (semantics, project, enclosing, *children.values())
    }
    if len(span_ids) != len(children) + 3:
        raise ValueError("project fact phases reuse a span ID")
    project_time = phase_measurement(project)
    phase_times = {name: phase_measurement(span) for name, span in children.items()}
    nested = phase_measurement(enclosing)
    return children["analysis.static_admission"], {
        "instance": project["instance"],
        "files": int(dict(project["attributes"])["files"]),
        "project": project_time,
        "phases": phase_times,
        "enclosing_definitions": nested,
        "graph_admission_outside_enclosing_wall_ns": (
            phase_times["analysis.graph_admission"]["wall_ns"] - nested["wall_ns"]
        ),
        "unattributed_project_wall_ns": (
            project_time["wall_ns"]
            - sum(phase["wall_ns"] for phase in phase_times.values())
        ),
    }


def summarize(
    burst: dict,
    lines: Iterable[bytes],
    file_count: int = FILE_COUNT,
    *,
    decision_required: bool = True,
    project_attribution: bool = False,
) -> dict:
    revision = burst["settled"]["revision"]
    [match] = [p for p in burst["publications"] if p["revision"] == revision]
    if int(match["changed_count"]) != file_count:
        raise ValueError("final publication did not contain the changed paths")
    publications, projects, decisions, phases = [], [], [], []
    markers = (b"index snapshot published", PROJECT.encode(), DECISION.encode())
    if project_attribution:
        markers += tuple(name.encode() for name in (*PHASES, ENCLOSING, SEMANTICS))
    for line in lines:
        if not any(marker in line for marker in markers):
            continue
        row = json.loads(line)
        event = row["record"]
        attrs = dict(event.get("attributes", []))
        if row["kind"] == "span" and event["name"] == PROJECT:
            projects.append(event)
        if row["kind"] == "span" and event["name"] in (*PHASES, ENCLOSING, SEMANTICS):
            phases.append(event)
        if row["kind"] == "log record":
            if event["body"] == DECISION and attrs.get("operation") == DECISION:
                decisions.append(event)
            elif (
                event["body"] == "index snapshot published"
                and attrs.get("operation") == "index.publish"
                and attrs.get("trigger") == "filesystem"
                and attrs.get("epoch") == match["epoch"]
            ):
                publications.append(event)
    [publication] = publications
    instance = dict(publication["resource"])["service.instance.id"]
    end = publication["time_unix_nano"]
    # The corpus client reports the publication time in seconds to microsecond precision.
    start = end - round(match["seconds"] * 1e9) - 2_000_000
    [project] = [
        span
        for span in projects
        if dict(span["resource"])["service.instance.id"] == instance
        and start <= span["start_time_unix_nano"] < span["end_time_unix_nano"] <= end
    ]
    parent = project
    attribution = {}
    if project_attribution:
        parent, attribution = attribute_project(project, phases)
    matched_decisions = [
        event
        for event in decisions
        if dict(event["resource"])["service.instance.id"] == instance
        and parent["start_time_unix_nano"]
        <= event["time_unix_nano"]
        <= parent["end_time_unix_nano"]
        and (
            event.get("trace_id") == parent["trace_id"]
            or (not project_attribution and not event.get("trace_id"))
        )
        and (
            event.get("span_id") == parent["span_id"]
            or (not project_attribution and not event.get("span_id"))
        )
    ]
    publication_fact = {
        "epoch": match["epoch"],
        "revision": revision,
        "project_facts_seconds": (
            project["end_time_unix_nano"] - project["start_time_unix_nano"]
        )
        / 1e9,
    }
    if not decision_required:
        if matched_decisions:
            raise ValueError(
                "the control unexpectedly recorded a finalization decision"
            )
        return {**publication_fact, "mode": "unreported"}
    [decision] = matched_decisions
    attrs = dict(decision["attributes"])
    mode = attrs["mode"]
    keys = [
        "graph_outgoing_lists",
        "graph_incoming_lists",
        "graph_drops",
        "edges_max",
        "candidate_sources",
        "static_admitted",
        "skipped_outgoing_rows",
        "skipped_incoming_rows",
    ]
    if mode != "no_static_inputs":
        keys.extend(
            [
                "graph_rows",
                "static_drops",
                "remaining",
                "outgoing_lists",
                "outgoing_rows",
                "skipped_outgoing_lists",
                "incoming_lists",
                "incoming_rows",
                "skipped_incoming_lists",
            ]
        )
    counts = {key: int(attrs[key]) for key in keys}
    if min(counts.values()) < 0:
        raise ValueError("negative adjacency count")
    reasons = []
    if counts["graph_outgoing_lists"] + counts["graph_incoming_lists"] > 8192:
        reasons.append("original endpoint limit")
    if counts["graph_drops"]:
        reasons.append("graph drops")
    if mode == "no_static_inputs":
        if counts["candidate_sources"] or counts["static_admitted"]:
            raise ValueError("empty input reported admitted candidates")
        reasons.append("no static inputs")
    else:
        if counts["static_drops"]:
            reasons.append("static drops")
        if counts["remaining"] == 0:
            reasons.append("capacity filled")
        if counts["static_admitted"] == 0:
            reasons.append("no admitted static rows")
        expected = (
            "no_new_rows"
            if not counts["static_admitted"]
            else ("full" if reasons else "static_only")
        )
        if mode != expected:
            raise ValueError("finalization mode differs from admission counters")
        if (
            max(0, counts["edges_max"] - counts["graph_rows"])
            != counts["static_admitted"] + counts["remaining"]
        ):
            raise ValueError("static admission differs from remaining capacity")
        if counts["static_admitted"]:
            for direction in ("outgoing", "incoming"):
                if (
                    counts[f"{direction}_rows"] + counts[f"skipped_{direction}_rows"]
                    != counts["graph_rows"] + counts["static_admitted"]
                ):
                    raise ValueError(
                        f"{direction} finalization differs from admitted rows"
                    )
    if mode != "static_only" and (
        counts["skipped_outgoing_rows"] or counts["skipped_incoming_rows"]
    ):
        raise ValueError("fallback reported skipped rows")
    return {
        **publication_fact,
        "mode": mode,
        "fallback_reasons": reasons,
        "counts": counts,
        **({"project_attribution": attribution} if project_attribution else {}),
    }


def measure_project_facts(binary: Path, output: Path) -> None:
    allowed = sorted(psutil.Process().cpu_affinity())
    if len(allowed) < 4 or pins()["bun"].seconds != 540:
        raise ValueError(
            "project measurement requires four logical CPUs and a 540-second case"
        )
    sample = output / "candidate"
    measurement = run(binary, sample, ",".join(str(cpu) for cpu in allowed[:2]))
    report = json.loads((sample / "burst.json").read_bytes())
    if any(report["collector"]["dropped"].values()):
        raise ValueError("the trace collector dropped records")
    [burst] = [row for row in report["actions"] if row["action"] == "burst"]
    with gzip.open(sample / "burst.telemetry/telemetry.jsonl.gz", "rb") as records:
        attribution = summarize(burst, records, project_attribution=True)
    result = {**measurement, **attribution}
    (output / "project-facts.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(attribution), flush=True)


def main() -> None:
    if len(sys.argv) == 4 and sys.argv[1] == "--project-facts":
        binary, output = (Path(value).resolve(strict=True) for value in sys.argv[2:])
        measure_project_facts(binary, output)
        return
    baseline, candidate, output = (
        Path(value).resolve(strict=True) for value in sys.argv[1:]
    )
    binaries = {"base": baseline, "candidate": candidate}
    allowed = sorted(psutil.Process().cpu_affinity())
    if len(allowed) < 4:
        raise RuntimeError("the comparison requires four available logical CPUs")
    if pins()["bun"].seconds != 540:
        raise ValueError("the comparison requires the pinned 540-second case limit")
    cpus = ",".join(str(cpu) for cpu in allowed[:2])
    cases: tuple[EditCase, ...] = ("burst", "single-edit")
    results = {}
    for case in cases:
        expected = None
        file_count = FILE_COUNT if case == "burst" else 1
        for slot, label in enumerate(VARIANTS, 1):
            sample = output / f"{case}-{slot}-{label}"
            measurement = run(binaries[label], sample, cpus, case)
            fact = (measurement.pop("input"), measurement.pop("result"))
            if expected is None:
                expected = fact
            elif fact != expected:
                raise ValueError(f"{sample.name} differs in input or restored results")
            report = json.loads((sample / "burst.json").read_bytes())
            [burst] = [row for row in report["actions"] if row["action"] == "burst"]
            with gzip.open(
                sample / "burst.telemetry/telemetry.jsonl.gz", "rb"
            ) as records:
                finalization = summarize(
                    burst, records, file_count, decision_required=label == "candidate"
                )
            results[sample.name] = {**measurement, **finalization}
            (output / "pair.json").write_text(json.dumps(results, indent=2) + "\n")
            print(sample.name, json.dumps(results[sample.name]), flush=True)


if __name__ == "__main__":
    main()
