"""Bounded paired accepted-edit read over the exact locked standard artifacts."""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
import platform
import shutil
import sqlite3
import subprocess
import time
from contextlib import closing, suppress
from datetime import UTC, datetime
from pathlib import Path

import psutil
from attest import digest, download, extract, require
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
from rift_dev.rift_test_client import Server, verify_version

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
                "SELECT tree_revision FROM lexical_index_state"
            ).fetchone()
            if revision and saved == expected:
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


def exact(
    answer: dict, identity: str, marker: str, path_source: bytes, lexical_revision: str
) -> None:
    require(
        answer.get("outcome") == "found"
        and answer.get("symbol", {}).get("id") == identity,
        "the first request did not return its canonical marker",
    )
    revision = answer.get("revision", "")
    prefix = "rift://revision/tree/"
    require(
        revision.startswith(prefix)
        and len(revision) == len(prefix) + 64
        and revision[len(prefix) :].startswith(lexical_revision),
        "the response revision does not match the lexical digest prefix for the admitted edit",
    )
    require(
        not any(
            warning.get("code") == "stale_index"
            for warning in answer.get("warnings", [])
        ),
        "the read carries a stale-index warning",
    )
    needle = f"function {marker}() {{ return {marker.rsplit('_', 1)[1]}; }}".encode()
    start = path_source.index(needle)
    declarations = answer["declarations"]
    require(len(declarations) == 1, "positive singleton needs one actual declaration")
    source = declarations[0]
    require(
        source.get("path") == PROBE
        and source.get("range") == {"start": start, "end": start + len(needle)}
        and source.get("line") == path_source.count(b"\n", 0, start) + 1,
        "the canonical read points outside the edited file or exact bytes",
    )


def worker_operation(row: Line) -> str | None:
    if row.operation != "worker.run" or not row.label("busy") or not row.label("idle"):
        return None
    end = row.message_end("close")
    if end is None:
        return None
    # The ordinary renderer can split an unquoted multi-word work label at its first space.
    tail = row.rest[: end - len("close")].strip()
    return " ".join(part for part in (row.label("work"), tail) if part)


def observation(text: str, direct: bool) -> dict:
    records = list(lines(text))
    measured = [
        row for row in records if fields(row.context).get("tool") == "get_symbol"
    ]
    fingerprints = [
        row for row in measured if worker_operation(row) == "workspace fingerprint"
    ]
    reads = [
        row for row in measured if worker_operation(row) == "current workspace read"
    ]
    activations = [row for row in records if row.operation == DIRECT_EVENT]
    require(
        len(reads) == 1 and fingerprints,
        "first exact request did not yield one owning worker and a fingerprint",
    )
    require(
        len(activations) == int(direct),
        "direct branch did not match the requested profile and mode",
    )
    if direct:
        # Worker implementations may open their event outside the rendered request ancestry.
        # The single closed request interval and owning worker make attribution unambiguous.
        require(
            activations[0].time <= reads[0].time
            and activations[0].time >= fingerprints[-1].time,
            "direct event falls outside the measured confirmed request",
        )
    rows = fingerprints + reads + activations
    return {
        "direct": direct,
        "fingerprint_worker_seconds": [
            duration_seconds(row.label("busy")) for row in fingerprints
        ],
        "first_read_worker_seconds": duration_seconds(reads[0].label("busy")),
        "worker_trace": [row.text for row in sorted(rows, key=lambda item: item.time)],
    }


async def resource_guard(server: Server, output: Path) -> None:
    process = psutil.Process(server.process.pid)
    with output.open("x", encoding="utf-8") as logs:
        while True:
            stats = psutil.virtual_memory()
            cpu = psutil.cpu_percent()
            rss = process.memory_info().rss
            for child in process.children(recursive=True):
                with suppress(psutil.NoSuchProcess):
                    rss += child.memory_info().rss
            logs.write(
                json.dumps({"cpu": cpu, "ram": stats.percent, "rss": rss}) + "\n"
            )
            logs.flush()
            require(
                cpu < 90
                and stats.percent < 90
                and rss < 0.9 * stats.total
                and shutil.disk_usage(output.parent).free > DISK_FLOOR,
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
    marker, _, wire_id = declaration("bun", STEP[recipe])
    all_current = expected_digests({**original, **new})
    cold_started = time.perf_counter_ns()

    async def trial() -> dict:
        async with asyncio.timeout(COLD_MAX):
            debuted, _ = await wait_publication(server, 0, "startup")
            cold_lexical, original_revision = await wait_lexical(
                server, expected_digests(original)
            )
        async with server.connect(call_seconds=CALL_MAX) as client:
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
                if mode == "warm":
                    warm = await client.call(
                        "get_symbol",
                        {"id": wire_id, "include": {"documentation": True}},
                    )
                    exact(warm, wire_id, marker, new[PROBE], lexical_revision)
                pre_request = server.log_path.stat().st_size
                started = time.perf_counter_ns()
                answer = await client.call("get_symbol", {"id": wire_id})
                exact_ns = time.perf_counter_ns()
                write_json(destination / "answer.json", answer)
                exact(answer, wire_id, marker, new[PROBE], lexical_revision)
                trace = ""
                async with asyncio.timeout(3):
                    while True:
                        trace = log_since(server.log_path, pre_request)
                        if any(
                            worker_operation(row) == "current workspace read"
                            for row in lines(trace)
                        ):
                            break
                        await asyncio.sleep(0.01)
                measured = observation(trace, side == "candidate" and mode == "cold")
                revision = answer["revision"]
                if mode == "warm":
                    require(
                        warm["revision"] == revision,
                        "warm priming selected another revision",
                    )
                if side == "candidate" and mode == "cold":
                    prior = log_since(server.log_path, original_offset)
                    require(
                        sum(row.operation == DIRECT_EVENT for row in lines(prior)) == 1,
                        "the edit interval must have exactly one direct symbol request",
                    )
                snapshots = [
                    row
                    for row in lines(log_since(server.log_path, original_offset))
                    if row.is_message("index snapshot published")
                    and row.label("trigger") == "filesystem"
                ]
                require(
                    snapshots and snapshots[-1].time >= write_utc,
                    "no accepted publication after mutation",
                )
                facets = await client.call("facets", {"symbol": wire_id})
                require(
                    facets.get("revision") == revision,
                    "facets disagrees with the exact current revision",
                )
                require(
                    any(
                        marker in row["declaration"]["source"]["text"]
                        for row in facets["declarations"]
                    ),
                    "post-measurement facets does not hold the authored marker bytes",
                )

            async with asyncio.timeout(EDIT_MAX):
                restore_offset = server.log_path.stat().st_size
                for path in new:
                    (root / path).write_bytes(original[path])
                await wait_publication(server, restore_offset, "filesystem")
                _, restored_lexical = await wait_lexical(
                    server, expected_digests(original), lexical_revision
                )
                require(
                    restored_lexical == original_revision,
                    "raw file restore did not restore the lexical tree",
                )
                restored = await client.call("get_symbol", {"id": wire_id})
                absent = restored.get("outcome") == "missing" or (
                    restored.get("outcome") == "unavailable"
                    and restored.get("reason") == "insufficient_coverage"
                )
                require(
                    absent
                    and restored.get("revision", "").startswith(
                        "rift://revision/tree/" + original_revision
                    )
                    and not any(
                        warning.get("code") == "stale_index"
                        for warning in restored.get("warnings", [])
                    ),
                    "the restored revision still returns the marker or is unavailable without a current revision",
                )
                write_json(destination / "restore.json", restored)
                return {
                    "recipe": recipe,
                    "mode": mode,
                    "side": side,
                    "revision": revision,
                    "restored_revision": restored["revision"],
                    "lexical_tree": lexical_revision,
                    "cold_publication_seconds": (debuted - cold_started) / 1e9,
                    "cold_lexical_seconds": (cold_lexical - cold_started) / 1e9,
                    "write_seconds": (write_done - write_begin) / 1e9,
                    "first_publication_seconds": (published.result()[0] - write_begin)
                    / 1e9,
                    "final_publication_seconds": (
                        snapshots[-1].time - write_utc
                    ).total_seconds(),
                    "final_lexical_seconds": (lexical_ns - write_begin) / 1e9,
                    "first_exact_request_seconds": (exact_ns - started) / 1e9,
                    "write_to_exact_seconds": (exact_ns - write_begin) / 1e9,
                    **measured,
                }

    try:
        server.start(wait_for_publication=False)
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
            return await measurement
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
                state.rename(destination / "retired-state")


def paired(rows: list[dict], output: Path) -> dict:
    matched = {}
    for recipe, mode, order in ORDERS:
        named = [row for row in rows if row["recipe"] == recipe and row["mode"] == mode]
        require(len(named) == len(order), "one measurement pair did not finish")
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
                )
            )
        require(
            all(pair == wires[0] for pair in wires),
            "matched current/restore protocol differs between profiles",
        )
        matched[f"{recipe}-{mode}"] = {
            "answer_exact_match": True,
            "restore_exact_match": True,
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
    require(
        shutil.disk_usage(output).free > DISK_FLOOR, "insufficient artifact storage"
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
                artifacts[side], root, original, directory, recipe, mode, side
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
