"""Verify named normalization with the pinned Bun corpus and a supplied CLI."""

from __future__ import annotations

import asyncio
import hashlib
import json
import sqlite3
import sys
import time
from contextlib import AsyncExitStack, ExitStack, closing, suppress
from pathlib import Path
from typing import Any

import psutil
from rift_dev.commands import Command
from rift_dev.corpus_cache import git, pins
from rift_dev.log_records import lines
from rift_dev.perf.edit import (
    CONFIGURATION,
    LOG_FILTER,
    PATHS,
    declaration,
    log_since,
    read_edit,
    wait_publication,
)
from rift_dev.query import bindings, literal
from rift_dev.rift_test_client import Client, Server, ToolFailure, verify_version

SECONDS = 540
PROBE = PATHS["bun"]
TABLES = ("lexical_documents", "lexical_files", "documentation_sources")
PIN = "744846f844374847c902b5e7fd59b4342a51ef99"
SAVED_PATHS_SHA256 = "8e7bf05a11d7feaadcdb3d02232e8306c44aebbe24d2cd575e72769c50abb9ec"


def saved(destination: Path, value: object) -> None:
    destination.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def file_digest(data: bytes) -> bytes:
    return hashlib.sha256(data).digest()


def lexical(root: Path, selected: tuple[str, ...] = ()) -> dict[str, Any]:
    address = (root / ".rift" / "index").as_uri() + "?mode=ro"
    with closing(sqlite3.connect(address, uri=True, timeout=1)) as connection:
        revision = connection.execute(
            "SELECT tree_revision FROM lexical_index_state"
        ).fetchone()
        counts = {
            table: connection.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
            for table in TABLES
        }
        paths = {
            row[0]: bytes(row[1]).hex()
            for path in selected
            for row in connection.execute(
                "SELECT path, digest FROM lexical_files WHERE path = ?", (path,)
            )
        }
    if not revision or not all(counts.values()):
        raise RuntimeError("lexical store has not settled")
    return {"revision": revision[0], "counts": counts, "paths": paths}


def tracked(root: Path) -> list[str]:
    names = git(root, "ls-files", "-z", "--", "*.ts").with_timeout(10).output_bytes()
    uri = (root / ".rift" / "index").as_uri() + "?mode=ro"
    with closing(sqlite3.connect(uri, uri=True, timeout=5)) as connection:
        available = {
            row[0] for row in connection.execute("SELECT path FROM lexical_files")
        }
    return [
        path
        for name in names.decode().split("\0")
        if (path := name) in available and path != PROBE
    ]


def newline(data: bytes) -> bytes:
    if data.startswith(b"#!"):
        shebang, _, rest = data.partition(b"\n")
        return shebang + b"\n\n" + rest
    return b"\n" + data


def prepare(root: Path, output: Path) -> list[dict[str, Any]]:
    paths = tracked(root)
    chosen = [paths[len(paths) * index // 49] for index in range(49)] + [PROBE]
    if len(set(chosen)) != 50:
        raise AssertionError("recipe did not select 50 distinct indexed paths")
    list_hash = hashlib.sha256(("\n".join(chosen) + "\n").encode()).hexdigest()
    if list_hash != SAVED_PATHS_SHA256:
        saved(output / "unexpected-paths.json", chosen)
        raise AssertionError(f"selected paths changed: {list_hash}")
    original = {path: (root / path).read_bytes() for path in chosen}
    edits: list[dict[str, Any]] = []
    (output / "blobs").mkdir()
    for name, step, selected in (("single", 31, [PROBE]), ("burst", 50, chosen)):
        _, marker, _ = declaration("bun", step)
        rows = []
        for index, path in enumerate(selected):
            old = original[path]
            new = old + marker if path == PROBE else newline(old)
            if len(new) > 16 * 1024 * 1024:
                raise AssertionError(f"diagnostic source exceeds 16 MiB: {path}")
            old_path = f"blobs/{name}-{index}-original"
            new_path = f"blobs/{name}-{index}-updated"
            (output / old_path).write_bytes(old)
            (output / new_path).write_bytes(new)
            rows.append({"path": path, "original": old_path, "updated": new_path})
        edits.append({"name": name, "changes": rows})
        edits.append(
            {
                "name": f"{name}_restore",
                "changes": [
                    {
                        "path": row["path"],
                        "original": row["updated"],
                        "updated": row["original"],
                    }
                    for row in rows
                ],
            }
        )
    saved(output / "edits.json", edits)
    return edits


async def wait_store(
    root: Path,
    paths: tuple[str, ...],
    expected: dict[str, str] | None,
    before: str | None = None,
) -> dict[str, Any]:
    while True:
        try:
            snapshot = lexical(root, paths)
            if snapshot["revision"] != before and (
                expected is None or snapshot["paths"] == expected
            ):
                return snapshot
        except (RuntimeError, sqlite3.OperationalError):
            pass
        await asyncio.sleep(0.05)


async def symbol_read(
    client: Client, tool: str, symbol: str, marker: str, step: int
) -> dict[str, Any]:
    while True:
        try:
            if tool == "facets":
                return await read_edit(client, symbol, marker, step)
            if tool == "get_symbol":
                answer = await client.call(tool, {"id": symbol})
                if (
                    answer.get("outcome") == "found"
                    and isinstance(detail := answer.get("symbol"), dict)
                    and detail.get("id") == symbol
                    and answer.get("declarations")
                ):
                    return answer
            else:
                answer = await client.call(
                    tool,
                    {"query": f"SELECT ?name {{ <{symbol}> a :Symbol; :name ?name }}"},
                )
                if [literal(row, "name") for row in bindings(answer)] == [marker]:
                    return answer
        except ToolFailure as failure:
            if failure.retry != "same_request" and not missing_declaration(
                failure, symbol
            ):
                raise
        await asyncio.sleep(0.025)


def missing_declaration(failure: ToolFailure, symbol: str) -> bool:
    return (
        failure.message == f"requested symbol has no declaration in the index: {symbol}"
    )


async def restored_reads(clients: dict[str, Client], symbol: str) -> dict[str, object]:
    try:
        await clients["facets"].call("facets", {"symbol": symbol})
    except ToolFailure as failure:
        if not missing_declaration(failure, symbol):
            raise
    else:
        raise AssertionError(f"restored facets still holds {symbol}")
    exact = await clients["get_symbol"].call("get_symbol", {"id": symbol})
    if exact.get("outcome") != "missing":
        raise AssertionError(f"restored get_symbol still holds {symbol}")
    search = await clients["search"].call(
        "search",
        {"query": f"SELECT ?name {{ <{symbol}> a :Symbol; :name ?name }}"},
    )
    if bindings(search):
        raise AssertionError(f"restored search still holds {symbol}")
    if search["revision"] != exact["revision"]:
        raise AssertionError("restored reads served different revisions")
    return {"missing": True, "revision": exact["revision"]}


async def timed_read(
    client: Client, tool: str, symbol: str, marker: str, step: int, start: int
) -> dict[str, object]:
    answer = await symbol_read(client, tool, symbol, marker, step)
    return {
        "elapsed_ns": time.perf_counter_ns() - start,
        "revision": answer["revision"],
    }


def activation(
    log: str, paths: int, expected_survivors: int | None = None
) -> dict[str, int]:
    closed = [record for record in lines(log) if record.closes()]
    normalized = [
        row for row in closed if row.operation == "Normalizer::normalize_project"
    ]
    projected = [
        row for row in closed if row.operation == "Normalizer::projected_records"
    ]
    proofs = [
        row
        for row in closed
        if row.operation == "Normalizer::project_records_after_edit"
    ]
    if normalized or len(projected) != 1 or len(proofs) != 1:
        raise AssertionError(
            f"normalization for {paths} paths: full={len(normalized)} "
            f"projection={len(projected)} proof={len(proofs)}"
        )
    result = {
        "units": int(proofs[0].label("modified_units")),
        "selected_rows": int(proofs[0].label("selected_rows")),
        "survivors": int(projected[0].label("survivor_anchor_checks")),
        "records": int(projected[0].label("projected_records")),
        "references": int(projected[0].label("current_references")),
        "edges": int(projected[0].label("current_edges")),
    }
    if (
        result["units"] != paths
        or not 1 <= result["selected_rows"] <= 8192
        or (
            expected_survivors is not None and result["survivors"] != expected_survivors
        )
    ):
        raise AssertionError(f"unexpected normalization counts: {result}")
    return result


async def resources(root: Path, pid: int, output: Path) -> None:
    process = psutil.Process(pid)
    while True:
        memory = psutil.virtual_memory()
        cpu = psutil.cpu_percent()
        try:
            rss = process.memory_info().rss
            children = process.children(recursive=True)
        except psutil.NoSuchProcess:
            return
        for child in children:
            with suppress(psutil.NoSuchProcess):
                rss += child.memory_info().rss
        with output.open("a", encoding="utf-8") as log:
            log.write(
                json.dumps(
                    {"cpu_percent": cpu, "memory_percent": memory.percent, "rss": rss}
                )
                + "\n"
            )
        if (
            cpu >= 90
            or memory.percent >= 90
            or rss >= 0.9 * memory.total
            or psutil.disk_usage(str(root)).free < (5 << 30)
        ):
            raise RuntimeError("resource bound reached; see resource samples")
        await asyncio.sleep(0.25)


async def run_server(binary: Path, root: Path, output: Path) -> list[dict[str, Any]]:
    (root / "rift.toml").write_text(CONFIGURATION, encoding="utf-8")
    started = time.perf_counter_ns()
    server = Server(
        binary,
        root,
        output / "server.log",
        startup_seconds=SECONDS,
        env={"RUST_LOG": LOG_FILTER, "NO_COLOR": "1"},
    )
    with ExitStack() as stack:
        server.start(wait_for_publication=False)
        stack.callback(server.close)
        guard = asyncio.create_task(
            resources(root, server.process.pid, output / "resources.jsonl")
        )

        async def measure(clients: dict[str, Client]) -> list[dict[str, Any]]:
            await wait_publication(server, 0, "startup")
            first = await wait_store(root, (), None)
            saved(
                output / "cold.json",
                {"ready_ns": time.perf_counter_ns() - started, **first},
            )
            edits = prepare(root, output)
            selected = tuple(row["path"] for row in edits[2]["changes"])
            canonical = lexical(root, selected)
            if len(canonical["paths"]) != 50:
                raise AssertionError("cold lexical store lacks a selected path")
            before = canonical
            results: list[dict[str, Any]] = []
            for edit in edits:
                name = edit["name"]
                changes = edit["changes"]
                paths = tuple(row["path"] for row in changes)
                step = 31 if name.startswith("single") else 50
                restored = name.endswith("restore")
                marker, _, symbol = declaration("bun", step)
                expected = {
                    row["path"]: file_digest(
                        (output / row["updated"]).read_bytes()
                    ).hex()
                    for row in changes
                }
                offset = server.log_path.stat().st_size
                start = time.perf_counter_ns()
                for row in changes:
                    (root / row["path"]).write_bytes(
                        (output / row["updated"]).read_bytes()
                    )
                if not restored:
                    async with asyncio.TaskGroup() as tasks:
                        reads = {
                            tool: tasks.create_task(
                                timed_read(
                                    clients[tool], tool, symbol, marker, step, start
                                )
                            )
                            for tool in ("facets", "get_symbol", "search")
                        }
                        publication = tasks.create_task(
                            wait_publication(server, offset, "filesystem")
                        )
                        committed = tasks.create_task(
                            wait_store(root, paths, expected, before["revision"])
                        )
                    answers = {tool: task.result() for tool, task in reads.items()}
                    if (
                        len(
                            {
                                json.dumps(value["revision"], sort_keys=True)
                                for value in answers.values()
                            }
                        )
                        != 1
                    ):
                        raise AssertionError(f"{name} tools served different revisions")
                else:
                    async with asyncio.TaskGroup() as tasks:
                        publication = tasks.create_task(
                            wait_publication(server, offset, "filesystem")
                        )
                        committed = tasks.create_task(
                            wait_store(root, paths, expected, before["revision"])
                        )
                    answers = await restored_reads(clients, symbol)
                observed = committed.result()
                for row in changes:
                    if (
                        file_digest((root / row["path"]).read_bytes()).hex()
                        != expected[row["path"]]
                    ):
                        raise AssertionError(
                            f"{name} disk bytes differ for {row['path']}"
                        )
                if restored and lexical(root, selected) != canonical:
                    raise AssertionError(
                        f"{name} did not restore all lexical counts and digests"
                    )
                results.append(
                    {
                        "phase": name,
                        "publication_ns": publication.result()[0] - start,
                        "reads": answers,
                        "store": observed,
                        "paths": len(paths),
                        "log_offset": offset,
                        "log_end": server.log_path.stat().st_size,
                    }
                )
                saved(output / "server-measurements.json", results)
                phase_output = log_since(server.log_path, offset)
                results[-1]["normalization"] = activation(phase_output, len(paths))
                saved(output / "server-measurements.json", results)
                before = observed
            return results

        async def connected() -> list[dict[str, Any]]:
            async with AsyncExitStack() as sessions:
                clients = {
                    tool: await sessions.enter_async_context(
                        server.connect(log_path=output / f"{tool}.mcp.log")
                    )
                    for tool in ("facets", "get_symbol", "search")
                }
                return await measure(clients)

        measurement = asyncio.create_task(connected())
        try:
            done, _ = await asyncio.wait(
                (measurement, guard), return_when=asyncio.FIRST_COMPLETED
            )
            if guard in done:
                await guard
                raise RuntimeError("resource monitor stopped before the workload")
            result = measurement.result()
            server.stop()
            return result
        finally:
            for task in (measurement, guard):
                if not task.done():
                    task.cancel()
            await asyncio.gather(measurement, guard, return_exceptions=True)


def check_witness(witness: Path, root: Path, output: Path) -> int:
    results = {}
    for mode in ("named", "cold"):
        path = output / f"witness-{mode}.jsonl"
        time_path = output / f"witness-{mode}-time.txt"
        command = Command(
            "/usr/bin/time",
            "-v",
            "-o",
            time_path,
            witness,
            root,
            output / "edits.json",
            mode,
        ).with_timeout(SECONDS)
        with path.open("xb") as log, command.spawn(log) as running:
            deadline = time.monotonic() + SECONDS
            while running.poll() is None:
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"{mode} physical witness timed out")
                if psutil.cpu_percent() >= 90 or psutil.virtual_memory().percent >= 90:
                    raise RuntimeError(
                        f"{mode} physical witness reached the resource bound"
                    )
                time.sleep(0.25)
            if running.wait(1) != 0:
                raise RuntimeError(f"{mode} physical witness failed")
        results[mode] = {
            event["phase"]: event["witness"]
            for line in path.read_text(encoding="utf-8").splitlines()
            if (event := json.loads(line))
        }
    if len(results["cold"]) != 5 or results["named"] != results["cold"]:
        raise AssertionError("named and cold physical graph witnesses differ")
    return int(results["cold"]["cold"]["rows"])


def main() -> None:
    binary, witness, output = (Path(value).resolve() for value in sys.argv[1:])
    output.mkdir(parents=True, exist_ok=False)
    verify_version(binary, "0.0.92")
    pin = pins()["bun"]
    if pin.commit != PIN:
        raise AssertionError("unexpected Bun pin")
    root = output.parent / "bun-normalization-checkout"
    pin.checkout(root)
    with binary.open("rb") as executable:
        saved(
            output / "source.json",
            {
                "pin": pin.commit,
                "cli_sha256": hashlib.file_digest(executable, "sha256").hexdigest(),
            },
        )

    async def measure() -> list[dict[str, Any]]:
        async with asyncio.timeout(SECONDS):
            return await run_server(binary, root, output)

    results = asyncio.run(measure())
    survivors = check_witness(witness, root, output)
    with (output / "server.log").open("rb") as log:
        for event in results:
            log.seek(event["log_offset"])
            content = log.read(event["log_end"] - event["log_offset"])
            event["normalization"] = activation(
                content.decode("utf-8", errors="replace"), event["paths"], survivors
            )
    saved(output / "verified.json", {"survivors": survivors, "phases": results})


if __name__ == "__main__":
    main()
