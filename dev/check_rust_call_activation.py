"""Check whether the archived Rust lookup cache activates on pinned Bun edits."""

from __future__ import annotations

import asyncio
import gzip
import json
import sys
import time
from datetime import datetime
from pathlib import Path

import psutil
from check_burst_pair import run_guarded
from check_static_activation import PROJECT, SEMANTICS, child_span
from rift_dev.check_corpus import Corpus, await_startup_publication
from rift_dev.corpus_burst import FILE_COUNT, MARKER_PATH, run_burst
from rift_dev.corpus_cache import pins
from rift_dev.rift_test_client import Json, utc_now

LOOKUPS = "Index::rust_call_lookups"
PUBLISHED = "index snapshot published"
COUNTERS = (
    "native",
    "reused",
    "charged",
    "occurrences",
    "recorded",
    "retained_slots",
    "pruned_slots",
    "slot_bytes",
    "witness_files",
    "witness_rows",
)


class ActivationCorpus(Corpus):
    """Use one observed startup and two edit/restore pairs in one owned server."""

    async def cases(self, directory: Path) -> None:
        self.root = directory / "workspace"
        self.pin.checkout(self.root)
        self.configure()
        server = self.server()
        launched_at = utc_now()
        launched = time.perf_counter()
        with server:
            async with server.connect() as client:
                await await_startup_publication(server)
                self.record(
                    "burst_startup",
                    launched_at=launched_at,
                    publication_observed_seconds=time.perf_counter() - launched,
                )

                def record_seed(action: str, **values: Json) -> None:
                    self.record(f"seed_{action}", **values)

                await run_burst(
                    server, client, record_seed, 1, launched, observe_restore=True
                )
                await run_burst(
                    server,
                    client,
                    self.record,
                    FILE_COUNT,
                    launched,
                    observe_restore=True,
                )
            self.stop(server)


def read_trace(path: Path) -> tuple[list[dict], list[dict], list[dict]]:
    spans, publications, lookups = [], [], []
    with gzip.open(path, "rb") as source:
        for line in source:
            if not any(
                key in line
                for key in (
                    b"index.build",
                    SEMANTICS.encode(),
                    PROJECT.encode(),
                    PUBLISHED.encode(),
                    LOOKUPS.encode(),
                )
            ):
                continue
            row = json.loads(line)
            record = row["record"]
            if row["kind"] == "span" and record["name"] in (
                "index.build",
                SEMANTICS,
                PROJECT,
            ):
                spans.append(record)
            if row["kind"] != "log record":
                continue
            operation = dict(record["attributes"]).get("operation")
            if record["body"] == PUBLISHED and operation == "index.publish":
                publications.append(record)
            if record["body"] == LOOKUPS and operation == LOOKUPS:
                lookups.append(record)
    return spans, publications, lookups


def telemetry_window(
    publication: dict,
    written: str,
    count: int,
    revision: str,
    spans: list[dict],
    lookups: list[dict],
) -> dict:
    instance = dict(publication["resource"])["service.instance.id"]
    start = round(datetime.fromisoformat(written).timestamp() * 1e9)
    end = publication["time_unix_nano"]

    def inside(span: dict) -> bool:
        return span["instance"] == instance and (
            start
            <= span["start_time_unix_nano"]
            < span["end_time_unix_nano"]
            <= end + 1_000_000
        )

    if count:
        [build] = [
            span
            for span in spans
            if inside(span)
            and span["name"] == "index.build"
            and dict(span["attributes"]).get("tree_revision") == revision
            and int(dict(span["attributes"])["changed_count"]) == count
            and dict(span["attributes"]).get("outcome") == "ok"
        ]
        semantics = child_span(build, spans, SEMANTICS)
        project = child_span(semantics, spans, PROJECT)
    else:
        startup_projects = [
            span
            for span in spans
            if inside(span)
            and span["name"] == PROJECT
            and int(dict(span["attributes"])["files"]) > 0
        ]
        project = max(startup_projects, key=lambda span: span["end_time_unix_nano"])
        full_files = int(dict(project["attributes"])["files"])
        if any(
            span is not project and int(dict(span["attributes"])["files"]) >= full_files
            for span in startup_projects
        ):
            raise ValueError("startup has no unique final full project-facts build")
        [semantics] = [
            span
            for span in spans
            if span["name"] == SEMANTICS
            and span["instance"] == instance
            and span["trace_id"] == project["trace_id"]
            and span["span_id"] == project["parent_span_id"]
        ]
        child_span(semantics, [project], PROJECT)
    same_window = [
        event
        for event in lookups
        if dict(event["resource"])["service.instance.id"] == instance
        and project["start_time_unix_nano"] - 1_000_000
        <= event["time_unix_nano"]
        <= project["end_time_unix_nano"]
    ]
    if len(same_window) > 1:
        raise ValueError(
            "multiple Rust lookup events in a published project-facts span"
        )
    counts = {}
    if same_window:
        [event] = same_window
        if (event.get("trace_id"), event.get("span_id")) != (
            project["trace_id"],
            project["span_id"],
        ):
            raise ValueError(
                "Rust lookup event is not bound to the published project-facts span"
            )
        fields = dict(event["attributes"])
        counts = {k: int(fields[k]) for k in COUNTERS}
        if (
            min(counts.values()) < 0
            or counts["reused"] > counts["occurrences"]
            or counts["recorded"] > counts["retained_slots"]
            or counts["occurrences"] > counts["native"] + counts["reused"]
        ):
            raise ValueError("inconsistent Rust lookup counts")
    return {
        "epoch": dict(publication["attributes"])["epoch"],
        "revision": revision,
        "changed_count": count,
        "trace_id": project["trace_id"],
        "project_span_id": project["span_id"],
        "project_files": int(dict(project["attributes"])["files"]),
        "project_seconds": (
            project["end_time_unix_nano"] - project["start_time_unix_nano"]
        )
        / 1e9,
        "publication_seconds": (end - start) / 1e9,
        "lookup_state": "no_capture_event"
        if not counts
        else ("reused" if counts["reused"] else "no_hits"),
        "lookups": counts,
    }


def action_pair(
    rows: dict,
    prefix: str,
    count: int,
    publications: list[dict],
    startup_instance: str,
    spans: list[dict],
    lookups: list[dict],
) -> dict:
    inputs, wrote, edit, restore_wrote, restore = (
        rows[prefix + key]
        for key in (
            "burst_inputs",
            "burst_write",
            "burst",
            "burst_restore_write",
            "burst_restore",
        )
    )
    original, changed = inputs["original_sha256"], inputs["changed_sha256"]
    if (
        len(original) != count
        or original.keys() != changed.keys()
        or MARKER_PATH not in original
        or any(original[p] == changed[p] for p in original)
        or restore["disk_sha256"] != original
        or inputs["initial"] != restore["restored"]
        or not restore["map_equal"]
        or edit["files"] != count
    ):
        raise ValueError("wrong path set, persisted digests, or restored aggregate/map")
    measurements = []
    for clocks, outcome, name in (
        (wrote, edit, "settled"),
        (restore_wrote, restore, "restored"),
    ):
        revision = outcome[name]["revision"]
        [match] = [
            epoch for epoch in outcome["publications"] if epoch["revision"] == revision
        ]
        if int(match["changed_count"]) != count:
            raise ValueError(
                "the final epoch did not persist the complete changed path set"
            )
        [publication] = [
            pub
            for pub in publications
            if dict(pub["resource"])["service.instance.id"] == startup_instance
            and dict(pub["attributes"]).get("trigger") == "filesystem"
            and str(dict(pub["attributes"])["epoch"]) == str(match["epoch"])
        ]
        window = telemetry_window(
            publication, clocks["written_at"], count, revision, spans, lookups
        )
        measurements.append(
            {
                **window,
                "written_at": clocks["written_at"],
                "write_seconds": clocks["seconds"],
                "all_digests_seconds": outcome["all_digests_seconds"],
                "aggregate_rows": outcome[name],
            }
        )
    return {
        "input": {
            key: inputs[key] for key in ("original_sha256", "changed_sha256", "initial")
        },
        "edit": {
            **measurements[0],
            "facets_seconds": edit["facets_seconds"],
            "facets_revision": edit["facets"]["revision"],
            "facets_warnings": edit["facets"]["warnings"],
        },
        "restore": {
            **measurements[1],
            "map_equal": restore["map_equal"],
            "disk_sha256": restore["disk_sha256"],
        },
    }


def summarize(report: Path) -> dict:
    merged = json.loads(report.read_bytes())
    if (merged["commit"], merged["case"], merged["status"]) != (
        pins()["bun"].commit,
        "burst",
        "passed",
    ):
        raise ValueError("unverified Bun corpus case")
    raw = merged["telemetry_artifact"]
    if (
        raw["errors"]
        or raw["omitted"]
        or raw["received"] != raw["retained"]
        or any(raw["collector_dropped"].values())
    ):
        raise ValueError("cannot interpret lookup absence in a dropped trace")
    spans, publications, lookups = read_trace(Path(raw["path"]))
    rows = {row["action"]: row for row in merged["actions"]}
    if len(rows) != len(merged["actions"]):
        raise ValueError("duplicate action in activation sequence")
    [startup] = [
        pub
        for pub in publications
        if dict(pub["attributes"]).get("trigger") == "startup"
    ]
    instance = dict(startup["resource"])["service.instance.id"]
    initial = rows["seed_burst_lexical_ready"]
    boot = telemetry_window(
        startup,
        rows["burst_startup"]["launched_at"],
        0,
        initial["revision"],
        spans,
        lookups,
    )
    seed = action_pair(rows, "seed_", 1, publications, instance, spans, lookups)
    burst = action_pair(rows, "", FILE_COUNT, publications, instance, spans, lookups)
    epochs = [
        int(item["epoch"])
        for item in (
            boot,
            seed["edit"],
            seed["restore"],
            burst["edit"],
            burst["restore"],
        )
    ]
    if (
        epochs != sorted(set(epochs))
        or seed["input"]["initial"] != burst["input"]["initial"]
    ):
        raise ValueError("cases overlap or changed the initial aggregate state")
    native = burst["edit"]["lookups"].get("native", 0)
    reused = burst["edit"]["lookups"].get("reused", 0)
    return {
        "pin": merged["commit"],
        "machine": merged["machine"],
        "startup": {**boot, "cold_lexical_seconds": initial["seconds"]},
        "seed": seed,
        "burst": burst,
        # Run a matched control only for a cohort large enough to examine throughput.
        "matched_control_admitted": reused >= 1000 and reused * 100 >= native + reused,
    }


def main() -> None:
    mode, binary_argument, output = sys.argv[1:]
    binary = Path(binary_argument).resolve(strict=True)
    output = Path(output).resolve()
    if mode == "case":
        asyncio.run(ActivationCorpus(pins()["bun"], binary, output, "burst").run())
        return

    allowed = sorted(psutil.Process().cpu_affinity())
    if len(allowed) < 4:
        raise RuntimeError(
            "the activation comparison requires four available logical CPUs"
        )
    cpus = ",".join(str(cpu) for cpu in allowed[:2])
    if mode not in ("candidate", "control"):
        raise ValueError("choose the candidate or matched control")
    result_dir = output / mode
    report = result_dir / "activation.json"
    run_guarded(
        result_dir, cpus, [sys.executable, __file__, "case", str(binary), str(report)]
    )
    result = summarize(report)
    result["runner_cpus"] = cpus
    if mode == "control":
        candidate = json.loads((output / "candidate-summary.json").read_bytes())
        if (
            not candidate["matched_control_admitted"]
            or result["matched_control_admitted"]
        ):
            raise ValueError("unadmitted control or unexpected lookup events")
        if any(
            result[case]["input"] != candidate[case]["input"]
            for case in ("seed", "burst")
        ):
            raise ValueError(
                "the matched control did not start and edit the same source"
            )
        facts = ("revision", "aggregate_rows", "facets_revision", "facets_warnings")
        if any(
            result[case]["edit"][key] != candidate[case]["edit"][key]
            for case in ("seed", "burst")
            for key in facts
        ):
            raise ValueError(
                "candidate and control settled to different revisions or facts"
            )
        if any(
            result[case][phase]["lookups"]
            for case in ("seed", "burst")
            for phase in ("edit", "restore")
        ):
            raise ValueError("the baseline unexpectedly reported Rust lookup events")
    (output / f"{mode}-summary.json").write_text(json.dumps(result, indent=2) + "\n")
    print(
        mode,
        json.dumps({k: result[k] for k in ("startup", "matched_control_admitted")}),
        flush=True,
    )


if __name__ == "__main__":
    main()
