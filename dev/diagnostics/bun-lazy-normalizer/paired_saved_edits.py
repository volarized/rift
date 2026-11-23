"""Compare timed ordinary publications and exact scoped read results from two frozen CLI artifacts."""
from __future__ import annotations

import argparse
import asyncio
from collections.abc import Mapping
from contextlib import suppress
from datetime import UTC, datetime
import hashlib
import json
from pathlib import Path
import shutil
import time

from jsonschema import Draft202012Validator
import psutil

import wire

COLD_TIMEOUT = 115
EDIT_TIMEOUT = 75
CALL_TIMEOUT = 45
SERVER_TIMEOUT = 315
ORDER = "ABBAAB"
ROOT_ID = wire.declaration("bun", 31)[2].rsplit("/", 1)[0] + "/root"
USAGE_QUERY = (
    "SELECT ?unit ?start ?end { "
    f"<{ROOT_ID}> :usages ?use . "
    "?use :range [ :unit ?unit; :start ?start; :end ?end ] "
    "} ORDER BY ?unit ?start ?end"
)


def check(condition: bool, detail: str) -> None:
    if not condition:
        raise ValueError(detail)


def typed_packet(envelope: Mapping) -> tuple[str, dict | None]:
    """Classify the MCP union before opening fields that require a successful value."""
    structured = envelope.get("structuredContent")
    facts = structured if isinstance(structured, dict) else None
    if envelope.get("isError") is True:
        return "error", facts
    if facts is None:
        return "missing_payload", None
    if facts.get("outcome") in {"unavailable", "missing"}:
        return facts["outcome"], facts
    return "success", facts


def positive_row(answer: dict, facets: dict) -> dict:
    """Keep ordered semantic rows, excluding transport and warning prose sectors."""
    return {
        "symbol": answer["symbol"],
        "declarations": answer["declarations"],
        "facets": [
            {
                key: row[key]
                for key in ("declaration", "source", "range", "kind", "regions", "syntax")
                if key in row
            }
            for row in facets["declarations"]
        ],
    }


def warning_codes(*responses: dict) -> list[list[str]]:
    """Compare known warning kinds separately, while original bytes remain in each wire reply."""
    return [
        [entry["code"] for entry in response.get("warnings", []) if "code" in entry]
        for response in responses
    ]


def edge_rows(usages: dict, revision: str) -> dict:
    return {
        "revision": revision,
        "physical_usage_rows": usages["answer"]["bindings"],
        "warning_codes": warning_codes(usages),
    }


async def read_packet(client, server, stage: Path, label: str, tool: str, arguments: dict):
    schema = client.tools[tool]
    Draft202012Validator(schema.input_schema).validate(arguments)
    log_offset = server.log_path.stat().st_size
    start = time.perf_counter_ns()
    wire.write_json(stage / f"{label}.start.json", {"at_ns": start, "arguments": arguments})
    try:
        result = await client.session.call_tool(tool, arguments)
    except BaseException as failure:
        wire.write_json(stage / f"{label}.failure.json", {
            "failure": type(failure).__name__, "at_ns": time.perf_counter_ns(),
        })
        raise
    done = time.perf_counter_ns()
    envelope = result.model_dump(mode="json", by_alias=True, exclude_none=True)
    wire.write_json(stage / f"{label}.wire.json", envelope)
    wire.write_json(stage / f"{label}.clock.json", {"start_ns": start, "done_ns": done, "arguments": arguments})
    status, answer = typed_packet(envelope)
    trace = ""
    try:
        async with asyncio.timeout(3):
            while True:
                trace = wire.log_since(server.log_path, log_offset)
                if any(wire.request(row)[0] == tool and row.is_message("tool request completed")
                       for row in wire.lines(trace)):
                    break
                await asyncio.sleep(0.01)
    finally:
        (stage / f"{label}.request.log").write_text(trace, encoding="utf-8")
    stale = any(row.label("warning_code") == "stale_index" for row in wire.lines(trace))
    tool_trace = None if stale else wire.verify_request(trace, tool)
    if stale:
        status = "stale"
    if status == "success":
        Draft202012Validator(schema.output_schema).validate(answer)
    return status, answer, start, done, tool_trace


async def check_read(client, server, stage: Path, mutated_at: int, prefix: str,
                     marker: str, identity: str, read_source: bytes, *, restored: bool) -> dict:
    attempts = []
    start = time.perf_counter_ns()
    async with asyncio.timeout(CALL_TIMEOUT):
        for i in range(4):
            status, answer, began, ended, request = await read_packet(
                client, server, stage, f"symbol-{i}", "get_symbol", {"id": identity},
            )
            attempts.append({"status": status, "start_ns": began, "done_ns": ended,
                             "request": request, "slug": (answer or {}).get("code")})
            wire.write_json(stage / f"attempt-{i}.json", attempts[-1])
            if restored and status != "stale" and answer is not None and answer.get("outcome") in {"missing", "unavailable"}:
                wire.restore_outcome(answer, status == "error", identity)
                break
            if not restored and status == "success":
                span = wire.exact(answer, identity, marker, read_source, prefix)
                break
            if status not in {"unavailable", "error", "stale"}:
                raise ValueError("exact read returned an unexpected outcome")
            await asyncio.sleep(0.05)
        else:
            raise ValueError("no verified current response within four saved attempts")
        workspace = await client.resource(wire.WORKSPACE_RESOURCE)
        wire.write_json(stage / "workspace.json", workspace)
        revision = wire.current_revision(workspace, prefix)
        if "revision" in answer or not restored:
            check(wire.current_revision(answer, prefix) == revision, "exact read has a different revision")
    output = {
        "first_raw_response_ns": attempts[0]["done_ns"], "verified_reply_ns": ended,
        "verification_observed_ns": time.perf_counter_ns(),
        "first_raw_from_write_ns": attempts[0]["done_ns"] - mutated_at,
        "verified_from_write_ns": ended - mutated_at, "first_request_schedule_ns": start,
        "revision": revision, "attempts": attempts,
    }
    wire.write_json(stage / "verified.json", output)
    return output | {"answer": answer, "span": None if restored else span}


async def declaration_rows(client, server, stage: Path, marker: dict, data: bytes) -> dict:
    answer = marker["answer"]
    identity = answer["symbol"]["id"]
    revision = marker["revision"]
    async with asyncio.timeout(CALL_TIMEOUT):
        status, facets, *_ = await read_packet(client, server, stage, "facets", "facets", {"symbol": identity})
        check(status == "success", "facets refused exact marker")
        wire.exact_facets(facets, identity, revision, marker["span"], data)
    canonical = positive_row(answer, facets)
    warning = warning_codes(answer, facets)
    wire.write_json(stage / "substantive.json", canonical)
    wire.write_json(stage / "warning-codes.json", warning)
    return {"substantive": canonical, "warning_codes": warning}


async def physical_rows(client, server, stage: Path, revision: str, prefix: str) -> dict:
    async with asyncio.timeout(CALL_TIMEOUT):
        status, usages, *_ = await read_packet(
            client, server, stage, "physical-usage", "search", {"query": USAGE_QUERY},
        )
        check(status == "success", "positive physical edge witness refused")
        check(wire.current_revision(usages, prefix) == revision, "physical edges use a different revision")
        value = usages.get("answer", {})
        page = usages.get("pagination", {})
        check(value.get("result") == "bindings" and value.get("bindings") and page.get("pages") == 1,
              "substantive physical edge control needs a complete positive page")
    output = edge_rows(usages, revision)
    wire.write_json(stage / "substantive.json", output)
    return output


async def publication_after(server, stage: Path, before: dict, after: dict, baseline: dict,
                            prior_epoch: int, old_prefix: str, recipe: str) -> tuple:
    for path, data in before.items():
        check((server.root / path).read_bytes() == data, f"wrong prior bytes in {path}")
    write_utc = datetime.now(UTC)
    started = time.perf_counter_ns()
    offset = server.log_path.stat().st_size
    for path, data in after.items():
        (server.root / path).write_bytes(data)
    finished = time.perf_counter_ns()
    expected = wire.expected_digests(baseline | after)
    check(len(expected) == 50 and len(after) == (1 if recipe == "single" else 50), "wrong saved edit cardinality")
    wire.write_json(stage / "physical-write.json", {"utc": write_utc.isoformat(), "begin_ns": started,
                    "done_ns": finished, "edits": wire.expected_digests(after), "lexical_digests": expected})
    async with asyncio.timeout(EDIT_TIMEOUT):
        async with asyncio.TaskGroup() as tasks:
            publication = tasks.create_task(wire.wait_publication(server, offset, "filesystem"))
            lexical = tasks.create_task(wire.wait_lexical(server, expected, old_prefix))
    first_observed, first = publication.result()
    lexical_observed, prefix = lexical.result()
    async with asyncio.timeout(3):
        while True:
            phase_log = wire.log_since(server.log_path, offset)
            trace = list(wire.lines(phase_log))
            settled = [
                row for row in trace
                if row.is_message("lexical commit settled")
                and (row.label("tree_revision") or "").startswith(prefix)
            ]
            if settled:
                break
            await asyncio.sleep(0.01)
    (stage / "publication.log").write_text(phase_log, encoding="utf-8")
    lexical_event = settled[-1]
    events = [row for row in trace
              if row.is_message("index snapshot published") and row.label("trigger") == "filesystem"
              and row.time <= lexical_event.time]
    check(bool(events), "no semantic publication before the matching lexical settle")
    last = events[-1]
    epoch = int(last.label("epoch"))
    check(int(first.label("epoch")) > prior_epoch and last.time >= first.time >= write_utc,
          "publication did not advance after the physical writes")
    check(wire.expected_digests({path: (server.root / path).read_bytes() for path in baseline}) == expected,
          "disk no longer matches the lexical transaction")
    wire.write_json(stage / "publication.json", {
        "epoch": epoch, "first_observed_ns": first_observed,
        "first_event": first.text, "complete_event": last.text, "lexical_digest_prefix": prefix,
        "lexical_event": lexical_event.text,
        "last_save_to_complete_seconds": (last.time - write_utc).total_seconds() - (finished - started) / 1e9,
        "last_save_to_lexical_ns": lexical_observed - finished,
        "lexical_observed_ns": lexical_observed, "lexical_digest_count": len(expected),
    })
    return epoch, prefix, finished


def process_memory(pid: int) -> dict:
    root = Path("/proc") / str(pid)
    return {
        key: int(value.split()[0]) * 1024
        for filename, keys in [("status", {"VmHWM", "VmRSS"}), ("smaps_rollup", {"Pss"})]
        for key, value in (
            line.split(":", 1) for line in (root / filename).read_text().splitlines() if ":" in line
        )
        if key in keys
    }


async def resources(server, stage: Path) -> None:
    proc = psutil.Process(server.process.pid)
    with (stage / "resources.jsonl").open("x", encoding="utf-8") as journal:
        psutil.cpu_percent()
        while True:
            mem = psutil.virtual_memory()
            cpu = psutil.cpu_percent()
            disk = shutil.disk_usage(stage).free
            pids = [proc, *proc.children(recursive=True)]
            parts = [capture for process in pids if (capture := observed_memory(process.pid))]
            rss = sum(value.get("VmRSS", 0) for value in parts)
            check(cpu < 90 and mem.percent < 90 and rss < .90 * mem.total and disk >= 5 << 30,
                  "host CPU, RAM, owned RSS or mounted disk limit")
            print(json.dumps({"at_ns": time.perf_counter_ns(), "cpu": cpu,
                              "ram": mem.percent, "bytes_free": disk, "process_memory": parts}),
                  file=journal, flush=True)
            await asyncio.sleep(0.25)


def observed_memory(pid):
    with suppress(FileNotFoundError, ProcessLookupError):
        return process_memory(pid)
    return None


async def trial(binary: Path, root: Path, original: dict, output: Path, edge_scope: str) -> dict:
    output.mkdir()
    for name in ("state", "cache", "config", "tmp"):
        (output / name).mkdir()
    state = root / ".rift"
    check(not state.exists(), "each trial requires a fresh workspace index")
    env = {"RUST_LOG": wire.LOG_FILTER, "NO_COLOR": "1", "RAYON_NUM_THREADS": "2",
           "XDG_STATE_HOME": str(output / "state"), "XDG_CACHE_HOME": str(output / "cache"),
           "XDG_CONFIG_HOME": str(output / "config"), "TMPDIR": str(output / "tmp")}
    server = wire.Server(binary, root, output / "server.log", startup_seconds=COLD_TIMEOUT, env=env)

    cold_started = time.perf_counter_ns()

    async def run():
        async with asyncio.timeout(COLD_TIMEOUT):
            published, row = await wire.wait_publication(server, 0, "startup")
            lexical, base_prefix = await wire.wait_lexical(server, wire.expected_digests(original))
        wire.write_json(output / "cold.json", {"start_ns": cold_started, "publication_ns": published,
                        "lexical_ns": lexical, "epoch": row.label("epoch"), "publication_event": row.text})
        epoch, prefix = int(row.label("epoch")), base_prefix
        result = {}
        async with server.connect(call_seconds=CALL_TIMEOUT) as client:
            base = await client.resource(wire.WORKSPACE_RESOURCE)
            wire.write_json(output / "base-workspace.json", base)
            base_revision = wire.current_revision(base, base_prefix)
            for recipe, number in wire.STEP.items():
                marker, _, identity = wire.declaration("bun", number)
                edited = wire.edited(original, recipe)
                prior = {path: original[path] for path in edited}
                for name, before, after in [(recipe, prior, edited), (f"{recipe}-restore", edited, prior)]:
                    stage = output / name
                    stage.mkdir()
                    epoch, prefix, physical = await publication_after(
                        server, stage, before, after, original, epoch, prefix, recipe,
                    )
                    restored = name.endswith("-restore")
                    read = await check_read(client, server, stage, physical, prefix,
                                            marker, identity, after[wire.PROBE], restored=restored)
                    check((read["revision"] == base_revision) == restored,
                          "current and restored tree revisions did not distinguish the exact state")
                    result[name] = {
                        "read": {key: val for key, val in read.items() if key not in {"answer", "span"}},
                        "publication": json.loads((stage / "publication.json").read_bytes()),
                    }
                    if not restored:
                        result[name].update(await declaration_rows(client, server, stage, read, after[wire.PROBE]))
                    wire.write_json(stage / "result.json", result[name])
            # A query retains a captured view. Observe it only after the last timed mutation.
            edge_stage = output / "untimed-edge-control"
            edge_stage.mkdir()
            edge_revision = base_revision
            if edge_scope != "base":
                edited = wire.edited(original, edge_scope)
                _, prefix, _ = await publication_after(
                    server, edge_stage, {path: original[path] for path in edited},
                    edited, original, epoch, prefix, edge_scope,
                )
                workspace = await client.resource(wire.WORKSPACE_RESOURCE)
                wire.write_json(edge_stage / "workspace.json", workspace)
                edge_revision = wire.current_revision(workspace, prefix)
                check(edge_revision == result[edge_scope]["read"]["revision"],
                      "untimed edge control has a different edited tree")
            result["edge_control"] = await physical_rows(client, server, edge_stage, edge_revision, prefix)
        return result

    done = False
    try:
        server.start(wait_for_publication=False)
        work = asyncio.create_task(run())
        guard = asyncio.create_task(resources(server, output))
        try:
            async with asyncio.timeout(SERVER_TIMEOUT):
                await asyncio.wait((work, guard), return_when=asyncio.FIRST_COMPLETED)
                if guard.done():
                    await guard
                    raise RuntimeError("resource guard exited")
                result = await work
            done = True
            return result
        finally:
            for task in (work, guard):
                if not task.done():
                    task.cancel()
            await asyncio.gather(work, guard, return_exceptions=True)
    finally:
        server.close()
        check(not hasattr(server, "process") or server.process.poll() is not None, "benchmark server was not reaped")
        for path, data in original.items():
            if (root / path).read_bytes() != data:
                (root / path).write_bytes(data)
        if state.exists():
            if done:
                shutil.rmtree(state)
            else:
                # Keep native state available on the runner without uploading a full index.
                preserved = root.parent / ("bun-lazy-failed-index-" + output.name)
                state.rename(preserved)
                wire.write_json(output / "state-preserved.json", {"directory": preserved.name})


async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("baseline", "candidate", "root", "lock", "paths", "output"):
        parser.add_argument("--" + key, type=Path, required=True)
    args = parser.parse_args()
    root, output = args.root.resolve(), args.output.resolve()
    check(not output.is_relative_to(root), "evidence must stay outside the corpus")
    lock = json.loads(args.lock.read_bytes())
    original = wire.fixture(root, args.paths, {"corpus_pin": lock["bun_pin"],
        "corpus_paths_sha256": lock["corpus_paths_sha256"]})
    configuration = root / "rift.toml"
    check(not configuration.exists(), "corpus already has a configuration")
    configuration.write_text(wire.CONFIGURATION, encoding="utf-8")
    output.mkdir()
    try:
        arms = {"A": args.baseline.resolve(), "B": args.candidate.resolve()}
        reference = {}
        edges = {}
        for index, slot in enumerate(ORDER):
            directory = output / f"{index + 1}-{slot}"
            edge_scope = ("single", "burst", "base")[index // 2]
            result = await trial(arms[slot], root, original, directory, edge_scope)
            wire.write_json(directory / "complete.json", result)
            for recipe in wire.STEP:
                candidate = (result[recipe]["read"]["revision"], result[recipe]["substantive"])
                if recipe not in reference:
                    reference[recipe] = candidate
                check(reference[recipe] == candidate, "published exact semantic row oracle changed")
            values = result["edge_control"]
            if edge_scope not in edges:
                edges[edge_scope] = values
            check(edges[edge_scope] == values, "published physical usage oracle changed")
        evidence = {"rows": reference, "edges": edges}
        wire.write_json(output / "passed.json", {"order": ORDER, "oracle": hashlib.sha256(json.dumps(evidence, sort_keys=True).encode()).hexdigest()})
    finally:
        configuration.unlink()


if __name__ == "__main__":
    asyncio.run(main())
