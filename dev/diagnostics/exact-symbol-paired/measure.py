"""Bounded paired accepted-edit read over the exact locked standard artifacts."""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
import platform
import re
import shutil
import sqlite3
import subprocess
import time
from contextlib import closing, suppress
from datetime import UTC, datetime
from pathlib import Path
from urllib.parse import quote

import psutil
from attest import digest, download, extract, require
from jsonschema import Draft202012Validator
from mcp import types
from rift_dev.log_records import Line, fields, lines
from rift_dev.perf.edit import (
    CONFIGURATION,
    LOG_FILTER,
    PATHS,
    declaration,
    log_since,
    wait_publication,
)
from rift_dev.perf.stages import duration_seconds
from rift_dev.rift_test_client import Client, MESSAGE_BYTES_MAX, Server, verify_version

PROBE = PATHS["bun"]
STEP = {"single": 31, "burst": 50}
# Counterbalance both cold sizes. The two warm controls independently reverse order.
ORDERS = (
    ("single", "cold", "ABBA"),
    ("burst", "cold", "BAAB"),
    ("single", "warm", "BA"),
    ("burst", "warm", "AB"),
)
WALL_MAX = 2100
COLD_MAX = 150
EDIT_MAX = 120
CALL_MAX = 30
DISK_FLOOR = 5 << 30
DIRECT_EVENT = "read.symbol.current_direct"
TREE_REVISION = re.compile(r"rift://revision/tree/[0-9a-f]{64}")
LEXICAL_PREFIX = re.compile(r"[0-9a-f]{8}")
WORKSPACE_RESOURCE = "rift://workspace?max_results=1"


def write_json(path: Path, value: object) -> None:
    with path.open("x", encoding="utf-8") as out:
        json.dump(value, out, indent=2, ensure_ascii=False)
        out.write("\n")


def git(root: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(root), *args], timeout=45)


def safe_input(path: str) -> bool:
    parsed = Path(path)
    return (
        not parsed.is_absolute() and ".." not in parsed.parts and path.endswith(".ts")
    )


def fixture(root: Path, saved_paths: Path, lock: dict) -> dict[str, bytes]:
    staged = saved_paths.read_bytes()
    require(
        hashlib.sha256(staged).hexdigest() == lock["corpus_paths_sha256"],
        "Bun changed path list does not match the saved 50-path recipe",
    )
    paths = staged.decode().splitlines()
    require(
        len(paths) == len(set(paths)) == 50
        and paths[-1] == PROBE
        and all(map(safe_input, paths)),
        "invalid Bun shape or unexpected mutation paths",
    )
    require(
        git(root, "rev-parse", "HEAD").decode().strip() == lock["corpus_pin"],
        "wrong Bun git pin",
    )
    require(
        not git(root, "status", "--porcelain", "--untracked-files=all"),
        "the dedicated Bun checkout is dirty",
    )
    tracked = {}
    for line in git(root, "ls-tree", "-z", "HEAD", "--", *paths).split(b"\0"):
        if line:
            left, path = line.decode().split("\t", 1)
            mode, kind, oid = left.split()
            require(
                mode in {"100644", "100755"} and kind == "blob",
                "selected source is not a regular git blob",
            )
            tracked[path] = oid
    original = {}
    for path in paths:
        file = root / path
        require(
            file.is_file() and not file.is_symlink() and file.resolve() == file,
            "unsafe selected source path",
        )
        value = file.read_bytes()
        require(
            len(value) < 16 << 20, "selected source exceeds the bounded witness size"
        )
        oid = hashlib.sha1(f"blob {len(value)}\0".encode() + value).hexdigest()
        require(
            tracked.get(path) == oid,
            f"working source differs from the locked git blob: {path}",
        )
        original[path] = value
    return original


def edited(original: dict[str, bytes], recipe: str) -> dict[str, bytes]:
    _, marker_source, _ = declaration("bun", STEP[recipe])
    if recipe == "single":
        return {PROBE: original[PROBE] + marker_source}
    result = {}
    for path, data in original.items():
        if path == PROBE:
            result[path] = data + marker_source
        else:
            offset = data.index(b"\n") + 1 if data.startswith(b"#!") else 0
            result[path] = data[:offset] + b"\n" + data[offset:]
    return result


def expected_digests(source: dict[str, bytes]) -> dict[str, str]:
    return {path: hashlib.sha256(data).hexdigest() for path, data in source.items()}


def lexical_state(root: Path, expected: dict[str, str]) -> str | None:
    uri = (root / ".rift" / "index").as_uri() + "?mode=ro"
    try:
        with closing(sqlite3.connect(uri, uri=True, timeout=0.1)) as db:
            db.execute("BEGIN")
            bound = ",".join("?" for _ in expected)
            saved = dict(
                db.execute(
                    f"SELECT path, lower(hex(digest)) FROM lexical_files WHERE path IN ({bound})",
                    tuple(expected),
                )
            )
            revision = db.execute(
                "SELECT tree_revision FROM lexical_index_state WHERE id = 1"
            ).fetchone()
            if revision and LEXICAL_PREFIX.fullmatch(revision[0] or "") and saved == expected:
                return revision[0]
    except sqlite3.OperationalError:
        return None
    return None


async def wait_lexical(
    server: Server, expected: dict[str, str], older: str | None = None
) -> tuple[int, str]:
    while True:
        server.check_running()
        if (revision := lexical_state(server.root, expected)) and revision != older:
            return time.perf_counter_ns(), revision
        await asyncio.sleep(0.025)


async def read_wire(
    client: Client, server: Server, destination: Path, name: str, tool: str, arguments: dict,
) -> tuple[dict, bool, int, int, str]:
    """Keep the reply and owning trace even when the accepted answer is a typed miss."""
    schema = client.tools[tool]
    Draft202012Validator(schema.input_schema).validate(arguments)
    offset = server.log_path.stat().st_size
    started_ns = time.perf_counter_ns()
    async with asyncio.timeout(client.call_seconds):
        result = await client.session.call_tool(tool, arguments)
    received_ns = time.perf_counter_ns()
    write_json(destination / f"{name}-wire.json", result.model_dump(
        mode="json", by_alias=True, exclude_none=True
    ))
    trace = ""
    try:
        async with asyncio.timeout(3):
            while True:
                trace = log_since(server.log_path, offset)
                if any(
                    request(row)[0] == tool and row.is_message("tool request completed")
                    for row in lines(trace)
                ):
                    break
                await asyncio.sleep(0.01)
    finally:
        (destination / f"{name}-request.log").write_text(trace, encoding="utf-8")
        write_json(destination / f"{name}-request.json", {
            "arguments": arguments, "started_ns": started_ns, "received_ns": received_ns,
        })
    require(isinstance(result, types.CallToolResult), "unexpected MCP call result")
    answer = result.structured_content
    require(isinstance(answer, dict), f"{tool} returned no structured result")
    write_json(destination / f"{name}.json", answer)
    require(
        len(json.dumps(answer).encode()) <= MESSAGE_BYTES_MAX,
        f"{tool} answer exceeded the message bound",
    )
    Draft202012Validator(schema.output_schema).validate(answer)
    verify_request(trace, tool)
    return answer, bool(result.is_error), started_ns, received_ns, trace


def current_revision(answer: dict, lexical_revision: str) -> str:
    revision = answer.get("revision", "")
    require(
        TREE_REVISION.fullmatch(revision) and LEXICAL_PREFIX.fullmatch(lexical_revision)
        and revision.removeprefix("rift://revision/tree/").startswith(lexical_revision),
        "the response does not name the full accepted working-tree revision",
    )
    require(
        all(warning.get("code") != "stale_index" for warning in answer.get("warnings", [])),
        "the read carries a stale-index warning",
    )
    return revision


def singleton(answer: dict) -> dict:
    declarations = answer["declarations"]
    page = answer["pagination"]
    require(
        len(declarations) == 1
        and (page["page"], page["pages"], page["total"]) == (1, 1, 1),
        "positive singleton needs one actual declaration across all pages",
    )
    return declarations[0]


def exact(
    answer: dict, identity: str, marker: str, path_source: bytes, lexical_revision: str,
) -> dict:
    require(
        answer.get("outcome") == "found"
        and answer.get("symbol", {}).get("id") == identity
        and answer["symbol"].get("name") == marker,
        "the first request did not return its canonical marker",
    )
    current_revision(answer, lexical_revision)
    needle = f"function {marker}() {{ return {marker.rsplit('_', 1)[1]}; }}".encode()
    require(path_source.count(needle) == 1, "marker bytes must occur once")
    start = path_source.index(needle)
    source = singleton(answer)
    require(
        source.get("path") == PROBE
        and "unit" not in source
        and source.get("origin", {}).get("location") == "project"
        and source.get("origin", {}).get("source_kind") == "authored"
        and source.get("range") == {"start": start, "end": start + len(needle)}
        and source.get("line") == path_source.count(b"\n", 0, start) + 1,
        "the canonical read points outside the authored file or exact bytes",
    )
    return source["range"]


def exact_facets(
    answer: dict, identity: str, revision: str, byte_range: dict, path_source: bytes,
) -> None:
    require(answer.get("symbol") == identity, "facets names another symbol")
    require(
        current_revision(answer, revision.removeprefix("rift://revision/tree/")[:8]) == revision,
        "facets disagrees with the exact current revision",
    )
    row = singleton(answer)
    source = row["source"]
    span = source["span"]
    excerpt = path_source[byte_range["start"]:byte_range["end"]]
    require(
        type(row["declaration"]) is int
        and row["declaration"] == 0
        and row["range"] == span["range"] == byte_range
        and span["unit"] == "rift://source/project/" + quote(PROBE, safe="/")
        and source["text"].encode("utf-8") == excerpt,
        "facets does not hold declaration zero at the exact authored source range",
    )


def restore_outcome(answer: dict, error: bool, identity: str) -> None:
    missing = (
        error and answer.get("outcome") == "missing"
        and answer.get("symbol_not_found", {}).get("id") == identity
    )
    unavailable = (
        not error and answer.get("outcome") == "unavailable"
        and answer.get("reason") == "insufficient_coverage"
        and answer.get("id") == identity
    )
    require(missing or unavailable, "restored marker did not return a typed absence")
    require(
        all(warning.get("code") != "stale_index" for warning in answer.get("warnings", [])),
        "the restored read carries a stale-index warning",
    )


def worker_operation(row: Line, message: str = "close") -> str | None:
    if row.operation != "worker.run":
        return None
    if message == "close" and (not row.label("busy") or not row.label("idle")):
        return None
    end = row.message_end(message)
    if end is None:
        return None
    if message != "close":
        return row.fields(message).get("work")
    # The renderer can split an unquoted multi-word work label after its first word.
    tail = row.rest[: end - len(message)].strip()
    return " ".join(part for part in (row.label("work"), tail) if part)


def request(row: Line) -> tuple[str | None, str | None]:
    root = fields(row.context)
    return root.get("tool"), root.get("req")


def verify_request(trace: str, tool: str) -> tuple[str | None, str | None]:
    records = list(lines(trace))
    starts = [row for row in records if row.is_message("tool request started")]
    ends = [row for row in records if row.is_message("tool request completed")]
    require(
        len(starts) == len(ends) == 1
        and request(starts[0]) == request(ends[0])
        and request(starts[0])[0] == tool
        and request(starts[0])[1] is not None
        and starts[0].time <= ends[0].time,
        f"{tool} does not have one complete owning request",
    )
    require(
        all(row.label("warning_code") != "stale_index" for row in records),
        "the server logged a stale-index warning during the read",
    )
    return request(starts[0])


def observation(text: str, direct: bool) -> dict:
    records = list(lines(text))
    reads = [row for row in records if worker_operation(row) == "current workspace read"]
    require(len(reads) == 1, "measured call needs one current workspace read worker")
    read = reads[0]
    owning_request = verify_request(text, "get_symbol")
    require(
        request(read) == owning_request,
        "current workspace read belongs to another request",
    )
    fingerprints = [
        row for row in records
        if worker_operation(row) == "workspace fingerprint" and request(row) == owning_request
    ]
    starts = [
        row for row in records
        if worker_operation(row, "worker execution started") == "current workspace read"
        and request(row) == owning_request
    ]
    require(
        len(starts) == 1 and fingerprints and fingerprints[-1].time <= starts[0].time <= read.time,
        "first exact request did not yield an ordered fingerprint and read worker",
    )
    activations = [row for row in records if row.operation == DIRECT_EVENT]
    require(
        len(activations) == int(direct),
        "direct branch did not match the requested profile and mode",
    )
    if direct:
        start_index = records.index(starts[0])
        end_index = records.index(read)
        activation = activations[0]
        require(
            request(activation) == owning_request
            and start_index < records.index(activation) < end_index
            and starts[0].time <= activation.time <= read.time,
            "direct event falls outside the measured read worker",
        )
    rows = fingerprints + starts + reads + activations
    return {
        "direct": direct,
        "request": owning_request[1],
        "direct_events": len(activations),
        "fingerprint_worker_seconds": [
            duration_seconds(row.label("busy")) for row in fingerprints
        ],
        "first_read_worker_seconds": duration_seconds(read.label("busy")),
        "worker_trace": [row.text for row in sorted(rows, key=records.index)],
    }


async def resource_guard(server: Server, output: Path) -> None:
    process = psutil.Process(server.process.pid)
    with output.open("x", encoding="utf-8") as logs:
        while True:
            stats = psutil.virtual_memory()
            cpu = psutil.cpu_percent()
            disk_free = shutil.disk_usage(output.parent).free
            rss = process.memory_info().rss
            for child in process.children(recursive=True):
                with suppress(psutil.NoSuchProcess):
                    rss += child.memory_info().rss
            logs.write(
                json.dumps(
                    {
                        "cpu": cpu,
                        "ram": stats.percent,
                        "rss": rss,
                        "disk_free": disk_free,
                    }
                )
                + "\n"
            )
            logs.flush()
            require(
                cpu < 90
                and stats.percent < 90
                and rss < 0.9 * stats.total
                and disk_free > DISK_FLOOR,
                "runner resource bound reached",
            )
            await asyncio.sleep(0.25)


async def one_trial(
    binary: Path,
    root: Path,
    original: dict[str, bytes],
    destination: Path,
    recipe: str,
    mode: str,
    side: str,
    *,
    expect_direct: bool,
    marker_documentation: bytes = b"",
) -> dict:
    require(
        not (root / ".rift").exists(), "a trial requires fresh Rift state and process"
    )
    destination.mkdir()
    for name in ("state", "cache", "config", "tmp"):
        (destination / name).mkdir()
    env = {
        "RUST_LOG": LOG_FILTER,
        "NO_COLOR": "1",
        "RAYON_NUM_THREADS": "2",
        "XDG_STATE_HOME": str(destination / "state"),
        "XDG_CACHE_HOME": str(destination / "cache"),
        "XDG_CONFIG_HOME": str(destination / "config"),
        "TMPDIR": str(destination / "tmp"),
    }
    server = Server(
        binary, root, destination / "server.log", startup_seconds=COLD_MAX, env=env
    )
    new = edited(original, recipe)
    if marker_documentation:
        _, marker_source, _ = declaration("bun", STEP[recipe])
        new[PROBE] = original[PROBE] + marker_documentation + marker_source
    marker, _, wire_id = declaration("bun", STEP[recipe])
    base_digests = expected_digests(original)
    edited_digests = expected_digests(new)
    all_current = {**base_digests, **edited_digests}
    cold_started = time.perf_counter_ns()
    write_json(destination / "edit-paths.json", {
        "recipe": recipe,
        "mode": mode,
        "side": side,
        "paths_changed": len(new),
        "base": base_digests,
        "written": edited_digests,
        "lexical_expected": all_current,
    })

    def checkpoint(
        name: str, wrote: datetime, publication: tuple[int, Line],
        lexical: tuple[int, str],
    ) -> None:
        write_json(destination / f"{name}-state.json", {
            "write_utc": wrote.isoformat(),
            "publication": publication[1].text,
            "epoch": publication[1].label("epoch"),
            "publication_observed_ns": publication[0],
            "lexical_observed_ns": lexical[0],
            "lexical_digest_prefix": lexical[1],
        })

    async def trial() -> dict:
        async with asyncio.timeout(COLD_MAX):
            startup = await wait_publication(server, 0, "startup")
            write_json(destination / "startup-publication.json", {
                "publication": startup[1].text,
                "publication_observed_ns": startup[0],
                "cold_started_ns": cold_started,
            })
            cold_lexical, original_revision = await wait_lexical(server, base_digests)
            write_json(destination / "startup-lexical.json", {
                "lexical_observed_ns": cold_lexical,
                "lexical_digest_prefix": original_revision,
            })
        async with server.connect(call_seconds=CALL_MAX) as client:
            base_workspace = await client.resource(WORKSPACE_RESOURCE)
            write_json(destination / "startup-workspace.json", base_workspace)
            base_revision = current_revision(base_workspace, original_revision)
            async with asyncio.timeout(EDIT_MAX):
                original_offset = server.log_path.stat().st_size
                write_utc = datetime.now(UTC)
                write_begin = time.perf_counter_ns()
                for path, data in new.items():
                    (root / path).write_bytes(data)
                write_done = time.perf_counter_ns()
                async with asyncio.TaskGroup() as work:
                    published = work.create_task(
                        wait_publication(server, original_offset, "filesystem")
                    )
                    lexical = work.create_task(
                        wait_lexical(server, all_current, original_revision)
                    )
                lexical_ns, lexical_revision = lexical.result()
                checkpoint("edit", write_utc, published.result(), lexical.result())
                require(
                    published.result()[1].time >= write_utc
                    and int(published.result()[1].label("epoch"))
                    > int(startup[1].label("epoch")),
                    "no accepted filesystem publication after mutation",
                )
                first_read_offset = server.log_path.stat().st_size
                warm = None
                if mode == "warm":
                    warm, error, _, _, warm_trace = await read_wire(
                        client, server, destination, "warm-answer", "get_symbol",
                        {"id": wire_id, "include": {"documentation": True}},
                    )
                    require(not error, "warm priming did not return a successful read")
                    exact(warm, wire_id, marker, new[PROBE], lexical_revision)
                    warm_workers = observation(warm_trace, direct=False)
                    write_json(destination / "warm-workers.json", warm_workers)

                answer, error, started, exact_ns, trace = await read_wire(
                    client, server, destination, "answer", "get_symbol", {"id": wire_id}
                )
                measured = observation(trace, expect_direct)
                write_json(destination / "workers.json", measured)
                require(not error, "measured get_symbol returned a tool error")
                byte_range = exact(answer, wire_id, marker, new[PROBE], lexical_revision)
                revision = answer["revision"]
                if warm is not None:
                    require(
                        warm["revision"] == revision and warm_workers["request"] != measured["request"],
                        "warm priming did not precede a distinct request at the same revision",
                    )
                prior = log_since(server.log_path, original_offset)
                require(
                    sum(row.operation == DIRECT_EVENT for row in lines(prior))
                    == int(expect_direct),
                    "direct symbol requests do not match the edit and measurement interval",
                )
                snapshots = [
                    row for row in lines(prior)
                    if row.is_message("index snapshot published")
                    and row.label("trigger") == "filesystem"
                ]
                require(
                    snapshots and snapshots[-1].time >= write_utc,
                    "no accepted publication after mutation",
                )
                facets, error, _, _, _ = await read_wire(
                    client, server, destination, "facets", "facets", {"symbol": wire_id},
                )
                require(not error, "post-measurement facets returned a tool error")
                exact_facets(facets, wire_id, revision, byte_range, new[PROBE])
                edited_workspace = await client.resource(WORKSPACE_RESOURCE)
                write_json(destination / "edit-workspace.json", edited_workspace)
                require(
                    current_revision(edited_workspace, lexical_revision) == revision
                    and revision != base_revision,
                    "the exact read disagrees with the edited publication",
                )
                require(
                    expected_digests({path: (root / path).read_bytes() for path in original})
                    == all_current,
                    "changed bytes differ from the admitted lexical rows",
                )
                with server.log_path.open("rb") as log:
                    before_first_read = log.read(first_read_offset).decode("utf-8", errors="replace")
                require(
                    all(request(row)[0] not in {"get_symbol", "facets"}
                        for row in lines(before_first_read)),
                    "exact declaration directory was requested before the first read",
                )

            async with asyncio.timeout(EDIT_MAX):
                restore_offset = server.log_path.stat().st_size
                restore_utc = datetime.now(UTC)
                for path in new:
                    (root / path).write_bytes(original[path])
                async with asyncio.TaskGroup() as work:
                    restored_publication = work.create_task(
                        wait_publication(server, restore_offset, "filesystem")
                    )
                    restored_lexical = work.create_task(
                        wait_lexical(server, base_digests, lexical_revision)
                    )
                restored_prefix = restored_lexical.result()[1]
                checkpoint("restore", restore_utc, restored_publication.result(), restored_lexical.result())
                require(
                    restored_publication.result()[1].time >= restore_utc
                    and int(restored_publication.result()[1].label("epoch"))
                    > int(snapshots[-1].label("epoch"))
                    and restored_prefix == original_revision,
                    "raw file restore did not publish the original lexical tree",
                )
                require(
                    expected_digests({path: (root / path).read_bytes() for path in original})
                    == base_digests,
                    "restored file bytes differ from the original",
                )
                restored, error, _, _, _ = await read_wire(
                    client, server, destination, "restore", "get_symbol", {"id": wire_id}
                )
                restore_outcome(restored, error, wire_id)
                restored_workspace = await client.resource(WORKSPACE_RESOURCE)
                write_json(destination / "restore-workspace.json", restored_workspace)
                restore_revision = current_revision(restored_workspace, restored_prefix)
                require(restore_revision == base_revision, "the base revision was not restored")
                if restored.get("outcome") == "missing" or "revision" in restored:
                    require(
                        current_revision(restored, restored_prefix) == restore_revision,
                        "the typed absence disagrees with the restored publication",
                    )
                return {
                    "recipe": recipe,
                    "mode": mode,
                    "side": side,
                    "revision": revision,
                    "restored_revision": restore_revision,
                    "lexical_tree": lexical_revision,
                    "cold_publication_seconds": (startup[0] - cold_started) / 1e9,
                    "cold_lexical_seconds": (cold_lexical - cold_started) / 1e9,
                    "write_seconds": (write_done - write_begin) / 1e9,
                    "first_publication_seconds": (published.result()[0] - write_begin) / 1e9,
                    "final_publication_seconds": (
                        snapshots[-1].time - write_utc
                    ).total_seconds(),
                    "final_lexical_seconds": (lexical_ns - write_begin) / 1e9,
                    "first_exact_request_seconds": (exact_ns - started) / 1e9,
                    "write_to_exact_seconds": (exact_ns - write_begin) / 1e9,
                    **measured,
                }

    completed = False
    try:
        server.start(wait_for_publication=False)
        write_json(destination / "process.json", {"pid": server.process.pid})
        measurement = asyncio.create_task(trial())
        guard = asyncio.create_task(
            resource_guard(server, destination / "resources.jsonl")
        )
        try:
            await asyncio.wait(
                (measurement, guard), return_when=asyncio.FIRST_COMPLETED
            )
            if guard.done():
                await guard
                raise RuntimeError("resource sampler exited early")
            report = await measurement
            completed = True
            return report
        finally:
            for task in (measurement, guard):
                if not task.done():
                    task.cancel()
            await asyncio.gather(measurement, guard, return_exceptions=True)
    finally:
        try:
            server.close()
        finally:
            for path in new:
                if (root / path).read_bytes() != original[path]:
                    (root / path).write_bytes(original[path])
            state = root / ".rift"
            if state.exists() and (
                not hasattr(server, "process") or server.process.poll() is not None
            ):
                if completed:
                    # The raw evidence has its own directory and the next trial cannot use this state.
                    require(
                        state.is_dir() and not state.is_symlink(),
                        "unexpected trial state",
                    )
                    shutil.rmtree(state)
                else:
                    state.rename(destination / "failed-state")


def paired(rows: list[dict], output: Path) -> dict:
    matched = {}
    for recipe, mode, order in ORDERS:
        named = [row for row in rows if row["recipe"] == recipe and row["mode"] == mode]
        require(
            "".join("A" if row["side"] == "baseline" else "B" for row in named) == order,
            "one measurement pair did not finish in the planned order",
        )
        candidate = [row for row in named if row["side"] == "candidate"]
        baseline = [row for row in named if row["side"] == "baseline"]
        require(len(candidate) == len(baseline), "unbalanced profile count")
        wires = []
        for row in named:
            directory = output / row["evidence"]
            wires.append(
                (
                    json.loads((directory / "answer.json").read_text()),
                    json.loads((directory / "restore.json").read_text()),
                    row["restored_revision"],
                    json.loads((directory / "facets.json").read_text()),
                    json.loads((directory / "warm-answer.json").read_text()) if mode == "warm" else None,
                )
            )
        require(
            all(pair == wires[0] for pair in wires),
            "matched current, facets, warm, or restore protocol differs between profiles",
        )
        matched[f"{recipe}-{mode}"] = {
            "answer_exact_match": True,
            "restore_exact_match": True,
            "facets_exact_match": True,
            "warm_exact_match": True if mode == "warm" else None,
            "order": order,
            "baseline": baseline,
            "candidate": candidate,
        }
    return matched


async def measurement(lock: dict, root: Path, output: Path, saved_paths: Path) -> None:
    require(
        platform.system() == "Linux"
        and platform.machine() == "x86_64"
        and platform.freedesktop_os_release().get("VERSION_ID") == "24.04",
        "requires the same Ubuntu x86_64 runner class as the standard artifacts",
    )
    require(
        os.environ.get("GITHUB_ACTIONS") == "true"
        and os.environ.get("RUNNER_OS") == "Linux",
        "run the study in one approved hosted runner application",
    )
    require(
        output.is_absolute()
        and root.is_absolute()
        and output != root
        and output not in root.parents
        and root not in output.parents,
        "dedicated checkout and separate result directory are required",
    )
    scratch = Path(os.environ["RUNNER_TEMP"]).resolve()
    require(
        scratch in root.parents and scratch in output.parents,
        "the checkout and result directory must be owned runner scratch",
    )
    require(
        not (root / "rift.toml").exists() and not (root / ".rift").exists(),
        "corpus cannot carry prior configuration or Rift state",
    )
    original = fixture(root, saved_paths, lock)
    output.mkdir(parents=False)
    require(
        root.stat().st_dev == output.stat().st_dev,
        "the dedicated checkout and evidence must share a filesystem",
    )
    archive_bytes = sum(
        lock[side]["bytes"] + lock[side]["tar_bytes"] + lock[side]["binary_bytes"]
        for side in ("baseline", "candidate")
    )
    require(
        shutil.disk_usage(output).free > DISK_FLOOR + archive_bytes,
        "insufficient artifact storage",
    )
    artifacts = {}
    for side in ("baseline", "candidate"):
        directory = output / side
        directory.mkdir()
        archive, proof = await asyncio.to_thread(download, lock, side, directory)
        binary, integrity = await asyncio.to_thread(
            extract, archive, directory, lock[side]
        )
        write_json(directory / "provenance.json", {**proof, **integrity})
        artifacts[side] = binary
    # The artifact token is needed only by GitHub's read API. Neither CLI inherits it.
    os.environ.pop("GH_TOKEN", None)
    os.environ.pop("GITHUB_TOKEN", None)
    for binary in artifacts.values():
        verify_version(binary, lock["binary_version"])
    write_json(
        output / "run.json",
        {
            "runtime": platform.platform(),
            "cpu_count": os.cpu_count(),
            "ram_bytes": psutil.virtual_memory().total,
            "run_id": os.environ["GITHUB_RUN_ID"],
            "corpus": lock["corpus_pin"],
            "recipe_sha256": digest(saved_paths),
            "plan": ORDERS,
        },
    )
    with (root / "rift.toml").open("x", encoding="utf-8") as file:
        file.write(CONFIGURATION)
    rows = []
    for recipe, mode, order in ORDERS:
        for index, key in enumerate(order, start=1):
            side = "baseline" if key == "A" else "candidate"
            directory = output / f"{recipe}-{mode}-{index}-{side}"
            require(
                all(
                    (root / path).read_bytes() == data
                    for path, data in original.items()
                ),
                "previous trial did not preserve exact base bytes",
            )
            row = await one_trial(
                artifacts[side], root, original, directory, recipe, mode, side,
                expect_direct=side == "candidate" and mode == "cold",
            )
            row["evidence"] = directory.name
            rows.append(row)
            write_json(directory / "measurement.json", row)
            # Preserve only compact data in workflow output; raw traces remain per trial.
            print(
                json.dumps(
                    {key: value for key, value in row.items() if key != "worker_trace"}
                ),
                flush=True,
            )
    write_json(output / "verified.json", paired(rows, output))


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parsed = parser.parse_args()
    here = Path(__file__).resolve().parent
    lock = json.loads((here / "pair.lock.json").read_text())
    psutil.cpu_percent()
    async with asyncio.timeout(WALL_MAX):
        await measurement(
            lock,
            parsed.workspace.resolve(),
            parsed.output.resolve(),
            here / "bun-edit-paths.txt",
        )


if __name__ == "__main__":
    asyncio.run(main())
